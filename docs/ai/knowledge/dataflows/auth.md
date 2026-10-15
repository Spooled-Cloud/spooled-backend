# Dataflow: Auth / Tenant

## REST (`api/middleware/auth.rs`)

1. Read `Authorization: Bearer …` — **header only** (`authenticate_api_key`). Since **0.1.111** `?token=` / `?api_key=` on a general REST route is a `401`; the query fallback lives on a separate realtime router (see Realtime).
2. If token starts with `eyJ` → JWT path (signature, blacklist, **re-check** active/expiry/queues from DB).
3. Else → API key (`sp_` / legacy `sk_`) through `authenticate_api_key_token`
   (`src/api/middleware/auth.rs:421`) → `resolve_api_key` (`:480`); see "API key
   resolution" below. The same function serves `/auth/login` and gRPC.
4. Context carries `organization_id` + queue scope (`can_access_queue`).
5. A database failure while authenticating is `503 SERVICE_UNAVAILABLE`
   "Authentication is temporarily unavailable" (pool timeout/closed, I/O, TLS) or
   `500` "Authentication failed" (`auth_db_error` `:717`, code mapping `:40`). The
   sqlx text is logged, never returned (BEOPS-27; it used to be a 500 with the raw
   error, and the `is_active` recheck turned a DB error into "API key is inactive").
   Applies to the API-key path and both JWT-path queries; `/auth/login` keeps a plain
   500 for non-401 failures. `GET /schedules/{id}/history` answers a failed schedule
   lookup with "Failed to get schedule history" and logs the error
   (`src/api/handlers/schedules.rs:769`; it returned `e.to_string()`).

## Admin (`api/middleware/admin_auth.rs`)

`X-Admin-Key` compared to configured `ADMIN_API_KEY` (SHA-256 then constant-time compare).

## Email signup (`api/handlers/email_login.rs` `complete_signup`)

`POST /api/v1/auth/signup/complete` creates an org + API key, so it uses the same
registration controls as `POST /organizations`:

- `EMAIL_SIGNUP_ENABLED=false` → 403, even with an admin key.
- `REGISTRATION_MODE=open` → public.
- `REGISTRATION_MODE=closed` → matching `X-Admin-Key` (the marketing Pages Function
  `functions/api/auth/signup/complete.ts` attaches it). Missing/wrong key → 403 whose
  message says to finish on the website and that the signup token is still valid
  (the gate runs before the token is consumed).
- `invite` → 403 (not implemented).

0.1.111 rejected every non-`open` completion and ignored the admin header, which
broke SaaS (`REGISTRATION_MODE=closed`, `EMAIL_SIGNUP_ENABLED=true`).

Slug (0.1.123, FESF-17): `normalize_signup_slug` trims and lower-cases the slug, accepts
lowercase letters, digits and hyphens (not at either end) and refuses every name in
`RESERVED_SLUGS` (`src/models/organization.rs`, the list `POST /organizations` uses) with a 400
`VALIDATION_ERROR`. It runs before the one-time token is consumed, so a refused slug leaves the
token usable. `SLUG_REGEX` (leading letter, no `--`) is NOT applied here: the website suggests
slugs that start with a digit and `GET /organizations/check-slug` calls them valid. Pinned by
`email_login::tests::test_complete_signup_checks_the_slug_before_it_consumes_the_token`.
Length (0.1.124, W9-R1 fixed): `GET /organizations/check-slug` reports `valid: false` with
`Validation failed: Slug must be 3-50 characters` unless the trimmed, lower-cased slug is 3-50 characters
(`validate_signup_slug_length`, `SLUG_MIN_LEN` / `SLUG_MAX_LEN` in `organizations.rs`), the length the
signup request (`CompleteSignupRequest`, counted as sent, before the trim) and `POST /organizations`
(`CreateOrganizationRequest`) accept; it used to take 1-100. Signup and `POST /organizations` are
unchanged (`validate_slug` keeps its 1-100 for `POST /organizations`, behind the request's 3-50). The
website trims before both calls, so the two agree; a client that pads a 50-character slug with spaces
still gets a green check and a 400 at signup. A reserved or taken slug's `suggestion` is cut so it stays
within 50 (`with_random_suffix`).
`POST /organizations/generate-slug` (0.1.124, W9-R2 fixed): `slug_base_for_name` keeps ASCII letters
and digits, one hyphen per other run, `org-` before a leading digit, at most 50 characters;
`base_needs_suffix` (shorter than 3, or in `RESERVED_SLUGS`) or a taken base gets `-` and six random hex
characters (base cut to 43 first), and an empty base (no ASCII letter or digit) becomes `org-` and the
suffix. Every result passes `check-slug`, signup and `POST /organizations` (`SLUG_REGEX` included):
`organizations::tests::test_generate_slug_*`, `test_check_slug_length_matches_signup`,
`r0124::r0124_generate_slug_never_offers_a_reserved_or_empty_slug`,
`r0124::r0124_check_slug_applies_the_signup_length`.

`GET /api/v1/auth/check-email` returns `available`, `exists`, and `signup_enabled`
(`EMAIL_SIGNUP_ENABLED`). The marketing signup page gates on `signup_enabled === false`
before sending a verification code.

## Sessions and logout (0.1.114)

Every login (`/auth/login`, email-login verify, signup complete) mints its access and
refresh token with a shared `sid` claim; `/auth/refresh` copies it onto new access
tokens. `POST /auth/logout` writes `session_revoked:{sid}` (TTL = refresh lifetime)
besides the access token's `token_blacklist:{jti}`, and `auth::is_token_revoked`
checks both in the JWT middleware, `/auth/refresh`, `/auth/me` and `/auth/validate`.
So logout ends the session even when the client does not send its refresh token.
Tokens minted before 0.1.114 have no `sid` and keep the old per-token behaviour.

## API key permissions

There are no per-permission scopes; `queues` is the only way to narrow a key.
`permissions` on create/update is a 400 `VALIDATION_ERROR` (0.1.114) — it used to be
dropped silently, so a "read-only" key had full access.

## API key queue scope: patterns (0.1.121, FEAT B2 / QW6)

A `queues` entry is `*` (every queue), an exact name, or `prefix.*` (every queue whose name starts with `prefix.`;
the bare parent `prefix` is not covered; case-sensitive). Any other use of `*` (`orders*`, `*.eu`, `a.*.b`, `.*`) is a
400 `VALIDATION_ERROR` (`invalid_queue_pattern`) from `validate_queues` (`models/api_key.rs:30`), run by
`POST /api-keys`, `PUT /api-keys/{id}` and the admin `POST /admin/organizations/{id}/api-keys`. Stored keys that hold
such an entry keep matching nothing (the deployment guide has the query).
- **One rule in Rust**: `queue_pattern_matches` (`:92`), `queue_scope_allows` (`:109`), `queue_scope_is_unrestricted`.
  `ApiKeyContext::can_access_queue` / `is_unrestricted` and `GrpcAuthContext` delegate to them; never re-inline
  `q == "*" || q == name`. Realtime fan-out and gRPC stream authorization call them too.
- **One rule in SQL**: `queue_in_key_scope(queue_name, scope)` (migration `20261013130100`, rewritten by
  `20261013130200`; NULL, empty or a `*` entry = every queue). Every row filter uses it, bound to
  `ctx.queue_scope_filter()` (NULL for an unrestricted key), and never `queue_name = ANY(scope)`, which hides every
  queue a pattern covers. Call sites: REST settle/heartbeat (`queue/mod.rs`), job, DLQ, workflow and schedule lists,
  dashboard and stats counts (`health.rs`), `GET /jobs/stats`, the workers list (a worker is listed only when every
  queue it serves is in scope: `NOT EXISTS (... NOT queue_in_key_scope(...))` over `queue_names`, else `queue_name`),
  gRPC unary settles, `GetJob`, and the StreamJobs/ProcessJobs claims and settles
  (`queue_in_key_scope(jobs.queue_name, k.queues)` on the locked key row).
- **Granting**: `can_grant_queues` lets a scoped key grant exact names it can use, and a `prefix.*` only under one of its
  own patterns at least as broad (`queue_scope_covers`); never `*`, never an empty list.
- **Planner**: `queue_scope_like_patterns()` is `IMMUTABLE` on purpose (`COST 100` stated), so a bound exact-name scope
  folds to `queue_name = ANY(...)` (index condition) and a prefix scope to a plain `LIKE ANY`; as a `STABLE` function it
  kept the pattern arm in every plan (a scoped key's dashboard count went from 1.5 ms to 43 ms at 130k jobs).

## Billing email: login identity, verification, uniqueness (0.1.117)

Email login resolves an org by its **verified**, lower-cased `billing_email`
(`email_login.rs` `VERIFIED_ORG_BY_EMAIL`, used by `check-email`, `start` and
`signup/complete`). `organizations.billing_email_verified_at` records the proof:
email signup sets it, `POST /organizations/billing-email/verification` +
`POST /organizations/billing-email/verify` set it, and an admin create/PATCH that
write of `billing_email` counts as verified (and, since 0.1.121, drops a pending change). `POST /organizations`
stores the address unverified. A wrong/expired confirmation code is 400 (not 401, which clients
read as an expired session). When a login code is verified for an address an
unconfirmed org holds, the signup response sets `unverified_org_exists: true`
(a hint only; it never links the caller to that org).
Before 0.1.117 any key holder could set someone else's address and capture that
person's later email "signup" into their org (BE-42). Existing addresses were
grandfathered as verified by migration `20261010120100` where email login could
already resolve them (stored lower-cased; the oldest org of a duplicate group).

**Pending billing email (0.1.121, W1-14).** `PUT /organizations/{id}` with a different address on an org whose
billing email is verified stores it in `organizations.pending_billing_email` (migration `20261013130000`) and leaves
`billing_email` and `billing_email_verified_at` alone: the old address keeps resolving email login and stays the
Stripe address. `POST /organizations/billing-email/verification` sends the code to the pending address when one is
set (`billing_email_to_verify`, `email_login.rs:1695`); `.../verify` (`:1808`) then sets `billing_email = pending`,
`billing_email_verified_at = NOW()`, `pending_billing_email = NULL` in one UPDATE guarded on the pending value.
Sending the active address again withdraws the change. An org without a verified address is changed directly
(unverified, no pending), as before. An admin `PATCH` that writes `billing_email` counts as verified and clears
pending (`admin.rs` `update_organization`). `GET`/`PUT /organizations/{id}` return `pending_billing_email`
(`OrganizationWithPendingEmail`), and the signup hint `unverified_org_exists` also matches a live org's pending
address. Open (cross-repo): the dashboard must show the pending address and offer send/verify while it is set,
even though `billing_email_verified_at` is non-null.

Verification codes live in `email_login_codes` with a `verify:` prefix and the
org id; the login `verify()` query excludes them so a confirmation code can never
log its recipient into the requesting org. They share the per-address hourly
budget with login codes.

Uniqueness (`organizations::billing_email_taken`) only counts verified addresses
among live orgs: an unverified copy never resolves a login, so it must not block
the real owner from signing up. Before 0.1.114 admin create/update accepted
duplicates and the login landed in an arbitrary org.

## Empty secrets

Optional secrets are read with `config::non_empty_env`: an empty or blank value
(compose `${NAME:-}`) is unset. Before 0.1.117 an empty `ADMIN_API_KEY` let an
empty `X-Admin-Key` header pass closed registration and signup, and an empty
`STRIPE_BILLING_WEBHOOK_SECRET` verified forged webhooks (BE-43). The gates also
refuse an empty configured key explicitly.

## Plan rate limiter and `X-RateLimit-*` headers (0.1.122, QW9, W8-R1)

`plan_rate_limit_middleware` (`api/middleware/plan_rate_limit.rs:291`) runs on the protected and realtime routers (JWT requests of the
dashboard too). Per request it checks (1) the organization's token bucket (`cache::check_token_bucket`, key `rate_limit:org:{org}`,
size = plan burst, refill = plan rps, or the `custom_limits` overrides) and then (2) the API key's own `rate_limit` (requests per
**minute**, fixed one-minute window, `cache::check_rate_limit`, key `rate_limit:api_key:{id}`), so a key limit can only lower the plan
limit (invariants #31). Public routes use a per-client limiter and admin routes their own; both answer `429` with `Retry-After` only.

- **Headers** (`RateLimitHeaders` `:92`, constants `HEADER_LIMIT` ... `HEADER_SCOPE` `:49-57`): `X-RateLimit-Limit`,
  `X-RateLimit-Remaining`, `X-RateLimit-Reset`, `X-RateLimit-Scope` (`organization` | `api_key`) on every authenticated response
  that passed the limiter and on its `429`. Organization scope: `Limit` = burst, `Remaining` = whole tokens left after this request
  (`from_token_bucket` `:101`), `reset_secs` = `ceil(next_token_ms / 1000)` (0 when the bucket is full). Key scope: `Limit` = the key's
  `rate_limit`, `reset_secs` = seconds to the window's `reset_at` (`from_window` `:111`). With both, `tighter` (`:137`) keeps the one
  with fewer remaining, ties to the later reset.
- **`X-RateLimit-Reset` is a Unix time in seconds** (`reset_at(now) = now + reset_secs`, `:149`; the current time while the bucket is
  full), not the wait. W8-R1: the first design sent the wait; all four SDKs parse the header as an epoch (a delta reads as January
  1970). `Retry-After` stays the wait. On a `429`, `rate_limit_exceeded_response` (`:170`) takes ONE clock reading for the body's
  `reset_at` and the header, so they are equal; `Remaining` is 0 and `Reset` is `now + Retry-After`.
- **No headers** when no check ran or one was skipped: public and admin routes, no Redis, a Redis error in fail-open mode
  (`bypassed`, because the numbers would be wrong), and the fail-closed `Rate limit unavailable` 429 (`Retry-After: 5` only).
  Clients must treat the headers as optional.
- **Token-bucket script** returns a 4th value, `next_token_ms` (`cache/mod.rs:762`, `TokenBucketResult::next_token_ms` `:790`): the
  time until `floor(tokens) + 1` accrues, 0 when full, equal to `retry_after_ms` on refusal. The hash layout is unchanged, so old and
  new replicas share buckets in a rolling deploy; the script has a new SHA (`EVALSHA` falls back to `EVAL` on `NOSCRIPT`). gRPC callers
  ignore the new field.
- **CORS** (`cors_exposed_headers` `api/mod.rs:85`, applied to the production and the development `CorsLayer`): `Access-Control-Expose-Headers`
  lists `x-ratelimit-limit`, `-remaining`, `-reset`, `-scope`, `retry-after`, `x-request-id`, `x-has-more`. None of them is CORS-safelisted,
  so without it the cross-origin dashboard (`PUBLIC_API_URL`) reads none. A new response header a browser client needs must be added there.

## Client IP: TCP peer, edge headers, IPv6 /64 (0.1.120, BEAUTH-10, BEBILL-24)

`main.rs` serves with `into_make_service_with_connect_info::<SocketAddr>()`, so each request carries its TCP
peer. `client_ip_from_peer(headers, peer, settings)` (`src/api/middleware/client_ip.rs:117`) is the one resolver;
the unreduced address (Turnstile `remoteip`) is `resolve_client_addr` (`:130`). Order of trust:

1. `TRUSTED_CLIENT_IP_HEADER`, believed from **any** peer (a request without it falls through to steps 3-4; the
   built-in edge headers are not consulted when it is set);
2. otherwise `CF-Connecting-IP`, then `X-Real-IP`, **only** when the peer is internal (`is_internal_peer` `:93`:
   loopback, 10/8, 172.16/12, 192.168/16, 169.254/16, `::1`, `fc00::/7`, `fe80::/10`; IPv4-mapped IPv6 is judged as
   IPv4) or unknown (`None`: a router driven without `ConnectInfo`, as the integration tests do);
3. `X-Forwarded-For` counted from the right past `TRUSTED_PROXY_HOPS` (never with the default 0);
4. the peer itself, then `unknown` (no peer and no header: tests only).

The result goes through `rate_limit_id` (`:177`): an IPv6 address becomes its /64 (`2001:db8:1:2::/64`), an
IPv4-mapped one plain IPv4, so rotating addresses inside a /64 mints no buckets. Callers: the public and admin
limiters (`plan_rate_limit.rs` `extract_client_id`), the failed-auth budget (`middleware/auth.rs:244`; `/auth/login`
`handlers/auth.rs`, which takes the `PeerAddr` extractor `client_ip.rs:80` that never rejects), `check-email`,
`check-slug` (`organizations.rs`), email start and verify. New code that limits by address must take `PeerAddr`
(handlers) or call `peer_addr(request.extensions())` (middleware) and use `client_ip_from_peer`; the old
two-argument `client_ip(headers, settings)` was removed because it treated the peer as unknown and kept the old
trust (a client reaching the backend directly could name its own bucket with `CF-Connecting-IP`).

Production: cloudflared reaches the backend over the docker network (a private peer), so `CF-Connecting-IP` is
believed with no configuration. `Settings::load` logs one `warn!` while `TRUSTED_CLIENT_IP_HEADER` is unset in
production (`client_ip_warning` `config/mod.rs:613`). Setting it to `CF-Connecting-IP` there is safe (the backend port
is not published). A self-hosted proxy that connects from a public address without the setting puts every caller in
its one bucket (runbook `docs/guides/runbooks/auth-token-issues.md`, Scenario H). Tests:
`w4_authrl_public_peer_cannot_choose_its_bucket_with_edge_headers`,
`w4_authrl_the_server_keys_a_bare_client_on_its_tcp_address`, `w4_authrl_check_slug_budget_follows_the_peer`.
**W4-05 (P3, open):** no peer-aware regression test for `/auth/login` or the REST failed-auth budget.

## Email login throttles, Turnstile, single-use codes (0.1.120, BEAUTH-06, -11, -24, FESF-15)

All Redis keys are fixed windows (`spend_budget` `email_login.rs:138`); `{ip}` is the `rate_limit_id`.

| Route | Key | Limit | Over the limit |
|-------|-----|-------|----------------|
| start | `email_start:minute:{ip}`, `email_start:hour:{ip}` | 3 per 60 s, 10 per 3600 s | 429 `RATE_LIMIT_EXCEEDED` + `Retry-After` + `details.retry_after_seconds` (`check_start_rate_limit` `:325`); a Redis error is open, or a 429 "temporarily unavailable" with `RATE_LIMIT_FAIL_CLOSED` |
| verify | `email_verify_attempts:{email}:{ip}` | 5 per 600 s | 429 "Too many attempts. Please try again in N minutes." |
| verify | `email_verify_ceiling:{email}` | 20 per 600 s, spent only after the strict budget passed | 429 "Too many attempts for this address. ..." |

(`spend_verify_attempt` `:876`, `too_many_requests` `:92`.) The DB cap of 5 codes per address per hour is unchanged
(429 without `Retry-After`). Before 0.1.120 a throttled verify was a 401 and the budget was 5 guesses per address from
everybody, so a stranger could lock a known email out (BEAUTH-06; the B-26 fix was incomplete).

- While the Redis budgets are enforced a login code tolerates 40 attempts (`MAX_ATTEMPTS_PER_LOGIN_CODE` `:46` = 2 x the
  ceiling), so guessing within the budgets never invalidates the owner's code: it slows, never blocks. Without Redis
  (`spend_verify_attempt` returns false; verify has no fail-closed mode) the per-code cap is 5 as before and a stranger
  can spend it. Trade-off: against a guesser spread over more than 4 client addresses the worst case rises from about 75
  to about 200 code comparisons per address per hour, out of 10^6 codes.
- **W4-04 (P3, open):** the 20 per 10 minutes per-address ceiling can still be burnt by one guesser who spans the /64s
  of an IPv6 /56 (256 x 5 guesses); the owner then waits out the window and no code is invalidated.
- Turnstile (`verify_turnstile` `:371`): checked only when `TURNSTILE_SECRET_KEY` is set (`settings.turnstile`,
  `config/mod.rs:308,591`; `verify_url` is the Cloudflare siteverify URL, a mock only in tests). It POSTs `secret`,
  `response`, `remoteip` as a form with a 10 s timeout; a missing, rejected or unverifiable token is 400
  `CAPTCHA_FAILED` (**fails closed**). The per-IP throttle runs first. Keep the secret unset until the site and the
  dashboard both send `turnstile_token` (max 2048 characters).
- Code rows are written **after** the mail send succeeds (login `start` and billing-email verification), so a provider
  outage no longer uses the hourly allowance. **W4-03 (P3, open):** that widens the check-then-insert race on the
  5 per hour cap, so concurrent starts for one address can send more than 5 mails (a hard fix needs a reservation row or
  an advisory lock). If the INSERT fails after a successful send, the user holds a code that never verifies (logged,
  500).
- A code is consumed with `UPDATE email_login_codes SET used_at = NOW() WHERE id = $1 AND used_at IS NULL` and
  `rows_affected` is checked (`:727`); the loser of a race gets 401 "Invalid or expired code" (BEAUTH-24), so parallel
  verifies of one code mint one session or one signup token.

## System API key: the email-login key (0.1.120, BEAUTH-08, BEBILL-09)

`api_keys.is_system` (migration `20261012110000`) marks keys Spooled creates itself: today only the "Email Login" key
behind dashboard sessions (prefix `sp_email`). `get_or_create_email_api_key` (`email_login.rs:1585`) finds it by
`is_system = TRUE AND is_active`, never by name (a customer key may be named "Email Login"); it is excluded from
`max_api_keys` (`get_org_resource_counts`), listed with `is_system: true`, and revoking it ends the dashboard session
as before. API create/update never write the flag. Residual: SELECT-then-INSERT with no lock or unique constraint, so
two concurrent verifies of different codes for one org can each create a system key (harmless, both are outside the
cap). During a rolling deploy an old pod can still insert an unmarked "Email Login" key; re-run the backfill in
`docs/guides/deployment.md`, "0.1.120 checks".

## API key resolution (0.1.119)

One implementation, `resolve_api_key` (`src/api/middleware/auth.rs:480`), behind
`authenticate_api_key_token` (`:421`: active/expiry checks + `touch_last_used`). Callers:
the REST middleware, `/auth/login` (`src/api/handlers/auth.rs:245`) and gRPC
(`src/grpc/auth.rs:182` `validate_api_key`). Each passes the `FailedAuthBudget` (`:109`) its
failures are charged to, which also picks its scan lane (`lane()` `:133`): `ClientIp` for REST
and login (lane `Rest`), `Grpc` for gRPC (lane `Grpc`). Order:

1. Redis record cache `api_key:{sha256(token)}` (60 s). A hit skips the database and
   bcrypt; revoke/update clears it for REST, login and gRPC alike.
2. Negative cache `auth_miss:{sha256}` (`AUTH_MISS_TTL_SECS` = 60 s, `:143`), written only
   after a legacy scan that ran found nothing.
3. Indexed `lookup_hash` row + one bcrypt verify (`find_by_lookup_hash` `:747`).
4. Legacy stage (`resolve_legacy` `:653`), for a token that missed 1-3:
   - **Single flight** (`LegacyFlight` `:582`, W3R-07): concurrent requests for one token on
     one lane share one scan. The first holds the flight's lock and leads; the others wait at
     most `LEGACY_FLIGHT_MAX_WAIT` = 20 s (`:576`) and take its outcome (found / missed / busy)
     without a charge or a scan of their own. A leader that leaves without an outcome
     (cancelled, over budget, database error) hands the lead to the next waiter.
   - **Budget charged before the scan** (`charge_failed_auth` `:905`, W3R-08): the leader
     INCRs the 60 s fixed-window counter, then compares it to the limit. REST and login:
     `FAILED_AUTH_PER_IP_LIMIT` = 20 (`:87`), Redis `auth_fail:{client_ip}`, keyed by
     `client_ip_from_peer` (TCP peer plus trusted edge header, an IPv6 caller as its /64; see
     "Client IP" above). gRPC: `FAILED_AUTH_GRPC_LIMIT` = 60 (`:98`), one Redis
     key `auth_fail_grpc` (`:102`) for every gRPC caller (W3R-06). Over the limit: 401, no scan,
     not negatively cached, stays charged. The charge is refunded (`refund_failed_auth` `:930`,
     `DECR`, key deleted at 0) when the scan or the final `lookup_hash` retry finds the key, the
     turn stays busy, or the scan hits a database error; it stays when the scan misses or the
     request is cancelled. So concurrent unknown tokens cannot all read the counter before any
     is charged, a client that hangs up mid-scan has paid, and a burst for a dormant legacy key
     costs its address nothing.
   - **Scan** (`scan_legacy_keys` `:782`, candidates `legacy_candidates` `:838`: active,
     `lookup_hash IS NULL`, the token's 8-char prefix or none, `LIMIT 100`) on the lane's turn
     (`bcrypt_gate::legacy_scan_turn`). A request that had to wait re-checks the record cache,
     the negative cache and `lookup_hash` first (the scan it waited for, on the other lane or
     another replica, may have resolved the token), then reads the candidates again. A match is
     backfilled (`backfill_lookup_hash` `:866`, awaited, as its own task so a cancelled request
     still completes it). If the turn stays busy for the bounded wait the result is `Busy`: 503
     `SERVICE_UNAVAILABLE` with `Retry-After: 5` (`AUTH_RETRY_AFTER_SECS` `:70`, added by
     `auth_error_response` `:44` to every auth 503) on REST and `/auth/login`, `UNAVAILABLE` on
     gRPC; never a 401, never negatively cached, never charged.
   - A miss gets one more `lookup_hash` retry, then the `auth_miss` write.

Constraints to keep (BEAUTH-05, BEGRPC-13, operator decision): legacy keys stay supported and
nothing is rotated or deactivated; the budget gates **only** step 4, a key that matches by the
record cache or `lookup_hash` is never charged or gated, even from an over-budget address;
negative-cache hits do not count toward the budget; Redis errors fail open for the caches and
the budgets. gRPC has no client address (the peer tonic sees is the ingress), so it shares one
bucket. Before W3R-06 it had no budget and a gRPC flood queued without bound on the one
process-wide scan turn; W3R-06's fix (a flat 2 s wait, then 401) still let one or two
sequential gRPC clients under the budget keep that shared turn busy for REST (W3R-08), and
refused valid legacy keys with 401 whenever a scan took longer than 2 s, which ten production
keys at cost 12 do (W3R-07). Since then each transport has its own turn.
Known limits: a key reactivated within 60 s of a failed attempt with its token stays refused
until `auth_miss` expires (revoke/update clears only the positive cache); with one shared
bucket, 20 bogus keys a minute from anyone (60 over gRPC) disable the legacy scan for every
caller of that bucket (modern keys unaffected); unknown tokens under a lane's budget can keep
that lane's turn busy, so its legacy keys may get a 503 (retry works, nothing is cached) but
the other lane's never do; a scan of close to 100 legacy rows plus the turn wait can exceed the
30 s request timeout; the REST budget is only as good as the client address (without
`TRUSTED_CLIENT_IP_HEADER`, `CF-Connecting-IP` / `X-Real-IP` count only from an internal peer;
any other caller is keyed on its own address). Production had 10
active legacy keys at the 2026-10-11 review; they stay supported (operator decision).

bcrypt gate (`src/api/middleware/bcrypt_gate.rs`): every `bcrypt::verify` on a request path goes
through `bcrypt_gate::verify` (`:60`): `spawn_blocking` behind a process-wide semaphore of
`(cores / 2).clamp(1, 4)` permits (`:22`, `:48`), the permit held inside the blocking task so a
cancelled request does not free its slot early. Each verify is timed into a moving average of
nanoseconds per unit of bcrypt work (`NANOS_PER_UNIT` `:36`, one unit = one of the `2^cost`
rounds, `verify_units` `:96`; seeded at 250 ms per cost-12 verify, `SEED_KEY_VERIFY` `:31`).
Legacy scans take turns per `ScanLane` (`:123`, `Rest` / `Grpc`), one scan per lane at a time:
`legacy_scan_turn` (`:217`) takes a free turn at once, otherwise waits `scan_wait` (`:193`) =
the larger of its own scan and the turn holder's (sum of candidate units × measured speed,
doubled while the other lane scans if there is a single verify permit) + 1 s
(`LEGACY_SCAN_WAIT_MARGIN` `:41`), at most 10 s (`LEGACY_SCAN_MAX_WAIT` `:45`); `Ok(None)` =
busy. A verify that cannot run is a 503, never "no match" (a valid key must not be negatively
cached). Throughput ceiling without Redis (every request a cache miss): about permits × 4
verifies/s per process at cost 12. A new bcrypt call on a request path must use the gate.

`/auth/login` used to carry its own copy of the lookup with inline bcrypt. It now shares the
cache, budget and REST lane, and refuses keys of soft-deleted orgs (`plan_tier = 'deleted'`)
with 401. A key it could not check (scan busy, database unreachable) is a 503
`service_unavailable` with `Retry-After` (`login` `src/api/handlers/auth.rs:168`), not a 500.
Its per-key `login_attempts` limiter and its `last_used` write are unchanged.

## API key bookkeeping

`api_keys.last_used` is written at most **once per key per 5 minutes**, not once per request (`touch_last_used` + Redis `SET NX` write guard `api_key_last_used:{id}`, `src/api/middleware/auth.rs:1020`, interval `:1005`). Deliberate write-amplification cap. Treat it as "was active in this 5-minute bucket", never as a live request timestamp. Since 0.1.119 gRPC uses the same debounced function (it used to spawn an UPDATE on every RPC).

## gRPC (`grpc/auth.rs`)

`x-api-key` or `authorization: Bearer` — **API-key path only** (no JWT). Soft-deleted orgs (`plan_tier <> 'deleted'`) excluded. Since 0.1.119 it authenticates through the REST function above (`validate_api_key` `:182`) with the shared Redis cache (`authenticate_request_with_cache` `:65`, `authenticate_from_metadata_with_cache` `:87`); a database outage or a legacy scan turn still busy after the bounded wait is `UNAVAILABLE`, not `INTERNAL` or `UNAUTHENTICATED`, so SDKs may retry. gRPC legacy scans take their own turn (lane `Grpc`). The "gRPC request authenticated" line is `debug!`. Open streams re-check the key with `reload_api_key_context` (`:216`) without bcrypt.

## Scoping

Handlers bind org from auth context; `require_queue_access` for queue ACL (patterns: see "API key queue scope" above; row filters use `queue_in_key_scope`). Isolation is application-level `WHERE organization_id = …`. RLS policies in DB are **inert** (see architecture guide) — do not rely on them as a backstop.

## Realtime

`GET /api/v1/events` (global SSE) forwards the org's Redis pub/sub events (`org:{id}:events`,
the same channel the WebSocket reads) as named SSE events, plus `system.health` every 10 s;
it is capped at 100 streams per org (all four streams share the lifetime rules below). Before 0.1.114 it
only emitted `system.health`. Published today by REST job create/bulk/complete/fail/cancel/
retry, plus (0.1.121) the scheduler's lease-expiry and offline-worker dead-lettering
(`Scheduler::announce_deadlettered`: `JobStatusChange` + `JobFailed`) — gRPC, schedules, workflows and webhook
ingest do not publish realtime events.

### Per-resource streams refuse before streaming (0.1.125, QAV2-L2)

`GET /events/jobs/{id}` looks the job up (`WHERE id AND organization_id`, then `can_access_queue`) before it takes a connection
slot or becomes an SSE response, and answers the `AppError::NotFound("Job not found")` of `GET /jobs/{id}` for an unknown id,
another organization's job and a job outside the key's queue scope (one indistinguishable 404; a failed lookup is a 500).
`GET /events/queues/{name}` answers the 403 of the REST queue routes (`require_queue_access`) for a queue outside the scope; a
queue in scope with no jobs yet is not a 404 (queues are implicit, the counts are the caller's own). Before 0.1.125 both opened
a 200 stream that only said `: connected` and closed, so a foreign id was an oracle. Order: 401 (`stream_session`), 404/403,
then 429 (`ConnectionSlot::admit`). A job deleted while a stream is open still ends it with an `error` event.

### Stream lifetime, session expiry and connection caps (0.1.121, BEFLOW-12/13/14/15, BEAUTH-14/19)

All four streams (WS, `/events`, `/events/jobs/{id}`, `/events/queues/{name}`) share `StreamAuth`
(`realtime.rs:588`) and `stream_session` (`:497`).
- `stream_session` re-reads the credential `authenticate_api_key_allow_query` accepted (Bearer header first, then the
  first non-empty `token` / `api_key` query value). For an access JWT it keeps `exp` / `jti` / `sid`, and it refuses a
  token whose `api_key_id` / `org_id` differ from the `ApiKeyContext`; org and scope still come only from the context.
  The JWT is decoded twice (middleware and here); an optional follow-up is passing `exp`/`jti`/`sid` on as a request
  extension from `authenticate_jwt_token`.
- A stream ends at the earlier of token acceptance end (`exp` + `Validation::default().leeway`, 60 s) and the maximum
  age (SSE 30 min `MAX_SSE_DURATION_SECS`, WS 1 h `MAX_WEBSOCKET_DURATION_SECS`).
- Revocation (`token_blacklist:{jti}`, `session_revoked:{sid}`) and the key, org and scope
  (`load_current_realtime_scope`) are re-checked every `SSE_AUTH_RECHECK_SECS` = 2 s at most: WS and `/events` before
  revealing anything (the WS also on every 30 s ping, never one query per event), the job stream on its 2 s poll, the
  queue stream on its 5 s poll. A narrowed queue scope takes effect then too.
- WS close codes (`StreamEnd`): `1008` token expired or revoked (an `Error` event first; the dashboard goes to
  `auth_failed` and reconnects when its token changes), `1000` maximum age reached, `1011` Redis pub/sub channel lost
  or never opened, or the authorization lookup failed (`Error` event first). The pub/sub stream is read in the main
  select loop, so its end is noticed; with Redis down WS clients reconnect with backoff instead of holding a silent
  socket.
- Per-org caps are Redis sorted sets `realtime_connections:{ws|events|job|queue}:{org}`
  (`realtime_connections_key` `:322`), 100 members per kind. Each member is a connection id scored with its heartbeat
  (unix seconds); admission is an atomic Lua script (`ADMIT_CONNECTION` `:328`): prune members older than 90 s
  (`CONNECTION_STALE_SECS`), count, `ZADD`; key TTL 180 s; a 30 s heartbeat; `ConnectionSlot` removes the member on drop;
  Redis errors or the 500 ms timeout admit (fail open). The old `ws_connections:{org}` / `sse_connections:{org}`
  counters are gone: they leaked on SIGKILL and the phantom count of an active org never decayed. During a rolling
  deploy old replicas count in the old keys, new ones in the sets, so the cap is briefly doubled; staleness uses the
  application clock, so replica clocks must agree within about a minute.
- Queue-stream counts come from a per-(org, queue) in-process single-flight cache (`QueueStatsCache` `:1169`,
  `QUEUE_STATS_CACHE_SECS` = 4), not one aggregate per stream every 5 s; with N replicas the aggregate runs at most N
  times per 4 s.
- Invariant: a stream must never outlive the token, session or key it was opened with by more than one re-check
  interval, and must never stay open after its event source is gone.

WS typically JWT via `?token=`; SSE may accept `?api_key=` (the Go SDK's SSE client relies on this). Since **0.1.111** the query-credential fallback is mounted ONLY on the four realtime routes — `authenticate_api_key_allow_query` in `src/api/middleware/auth.rs`, wired to its own router in `src/api/mod.rs`. All other protected routes use `authenticate_api_key`, which is header-only, closing the Referer/log leak on the general REST surface.
