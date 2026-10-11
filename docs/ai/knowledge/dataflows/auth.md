# Dataflow: Auth / Tenant

## REST (`api/middleware/auth.rs`)

1. Read `Authorization: Bearer …` — **header only** (`authenticate_api_key`). Since **0.1.111** `?token=` / `?api_key=` on a general REST route is a `401`; the query fallback lives on a separate realtime router (see Realtime).
2. If token starts with `eyJ` → JWT path (signature, blacklist, **re-check** active/expiry/queues from DB).
3. Else → API key (`sp_` / legacy `sk_`) through `authenticate_api_key_token`
   (`src/api/middleware/auth.rs:389`) → `resolve_api_key` (`:443`); see "API key
   resolution" below. The same function serves `/auth/login` and gRPC.
4. Context carries `organization_id` + queue scope (`can_access_queue`).
5. A database failure while authenticating is `503 SERVICE_UNAVAILABLE`
   "Authentication is temporarily unavailable" (pool timeout/closed, I/O, TLS) or
   `500` "Authentication failed" (`auth_db_error` `:532`, code mapping `:40`). The
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

## Billing email: login identity, verification, uniqueness (0.1.117)

Email login resolves an org by its **verified**, lower-cased `billing_email`
(`email_login.rs` `VERIFIED_ORG_BY_EMAIL`, used by `check-email`, `start` and
`signup/complete`). `organizations.billing_email_verified_at` records the proof:
email signup sets it, `POST /organizations/billing-email/verification` +
`POST /organizations/billing-email/verify` set it, and an admin create/PATCH that
write of `billing_email` counts as verified. `PUT /organizations/{id}` with a
different address clears it; `POST /organizations` stores the address
unverified. A wrong/expired confirmation code is 400 (not 401, which clients
read as an expired session). When a login code is verified for an address an
unconfirmed org holds, the signup response sets `unverified_org_exists: true`
(a hint only; it never links the caller to that org).
Before 0.1.117 any key holder could set someone else's address and capture that
person's later email "signup" into their org (BE-42). Existing addresses were
grandfathered as verified by migration `20261010120100` where email login could
already resolve them (stored lower-cased; the oldest org of a duplicate group).

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

## API key resolution (0.1.119)

One implementation, `resolve_api_key` (`src/api/middleware/auth.rs:476`), behind
`authenticate_api_key_token` (`:417`: active/expiry checks + `touch_last_used`). Callers:
the REST middleware, `/auth/login` (`src/api/handlers/auth.rs:240`) and gRPC
(`src/grpc/auth.rs:182` `validate_api_key`). Each passes the `FailedAuthBudget` (`:109`) its
failures are charged to, which also picks its scan lane (`lane()` `:133`): `ClientIp` for REST
and login (lane `Rest`), `Grpc` for gRPC (lane `Grpc`). Order:

1. Redis record cache `api_key:{sha256(token)}` (60 s). A hit skips the database and
   bcrypt; revoke/update clears it for REST, login and gRPC alike.
2. Negative cache `auth_miss:{sha256}` (`AUTH_MISS_TTL_SECS` = 60 s, `:143`), written only
   after a legacy scan that ran found nothing.
3. Indexed `lookup_hash` row + one bcrypt verify (`find_by_lookup_hash` `:743`).
4. Legacy stage (`resolve_legacy` `:649`), for a token that missed 1-3:
   - **Single flight** (`LegacyFlight` `:578`, W3R-07): concurrent requests for one token on
     one lane share one scan. The first holds the flight's lock and leads; the others wait at
     most `LEGACY_FLIGHT_MAX_WAIT` = 20 s (`:572`) and take its outcome (found / missed / busy)
     without a charge or a scan of their own. A leader that leaves without an outcome
     (cancelled, over budget, database error) hands the lead to the next waiter.
   - **Budget charged before the scan** (`charge_failed_auth` `:901`, W3R-08): the leader
     INCRs the 60 s fixed-window counter, then compares it to the limit. REST and login:
     `FAILED_AUTH_PER_IP_LIMIT` = 20 (`:87`), Redis `auth_fail:{client_ip}`, keyed by
     `api::middleware::client_ip` (returns `unknown` without trusted headers, so behind a bare
     proxy everyone shares one bucket). gRPC: `FAILED_AUTH_GRPC_LIMIT` = 60 (`:98`), one Redis
     key `auth_fail_grpc` (`:102`) for every gRPC caller (W3R-06). Over the limit: 401, no scan,
     not negatively cached, stays charged. The charge is refunded (`refund_failed_auth` `:926`,
     `DECR`, key deleted at 0) when the scan or the final `lookup_hash` retry finds the key, the
     turn stays busy, or the scan hits a database error; it stays when the scan misses or the
     request is cancelled. So concurrent unknown tokens cannot all read the counter before any
     is charged, a client that hangs up mid-scan has paid, and a burst for a dormant legacy key
     costs its address nothing.
   - **Scan** (`scan_legacy_keys` `:778`, candidates `legacy_candidates` `:834`: active,
     `lookup_hash IS NULL`, the token's 8-char prefix or none, `LIMIT 100`) on the lane's turn
     (`bcrypt_gate::legacy_scan_turn`). A request that had to wait re-checks the record cache,
     the negative cache and `lookup_hash` first (the scan it waited for, on the other lane or
     another replica, may have resolved the token), then reads the candidates again. A match is
     backfilled (`backfill_lookup_hash` `:862`, awaited, as its own task so a cancelled request
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
30 s request timeout; the REST budget is only as good as `client_ip` (without
`TRUSTED_CLIENT_IP_HEADER` it trusts `CF-Connecting-IP` / `X-Real-IP`). Production had 10
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

`api_keys.last_used` is written at most **once per key per 5 minutes**, not once per request (`touch_last_used` + Redis `SET NX` write guard `api_key_last_used:{id}`, `src/api/middleware/auth.rs:1016`, interval `:1001`). Deliberate write-amplification cap. Treat it as "was active in this 5-minute bucket", never as a live request timestamp. Since 0.1.119 gRPC uses the same debounced function (it used to spawn an UPDATE on every RPC).

## gRPC (`grpc/auth.rs`)

`x-api-key` or `authorization: Bearer` — **API-key path only** (no JWT). Soft-deleted orgs (`plan_tier <> 'deleted'`) excluded. Since 0.1.119 it authenticates through the REST function above (`validate_api_key` `:182`) with the shared Redis cache (`authenticate_request_with_cache` `:65`, `authenticate_from_metadata_with_cache` `:87`); a database outage or a legacy scan turn still busy after the bounded wait is `UNAVAILABLE`, not `INTERNAL` or `UNAUTHENTICATED`, so SDKs may retry. gRPC legacy scans take their own turn (lane `Grpc`). The "gRPC request authenticated" line is `debug!`. Open streams re-check the key with `reload_api_key_context` (`:216`) without bcrypt.

## Scoping

Handlers bind org from auth context; `require_queue_access` for queue ACL. Isolation is application-level `WHERE organization_id = …`. RLS policies in DB are **inert** (see architecture guide) — do not rely on them as a backstop.

## Realtime

`GET /api/v1/events` (global SSE) forwards the org's Redis pub/sub events (`org:{id}:events`,
the same channel the WebSocket reads) as named SSE events, plus `system.health` every 10 s;
it re-checks the key at most every 2 s and caps 100 streams per org. Before 0.1.114 it
only emitted `system.health`. Published today by REST job create/bulk/complete/fail/cancel/
retry only — gRPC, schedules, workflows and webhook ingest do not publish realtime events.

WS typically JWT via `?token=`; SSE may accept `?api_key=` (the Go SDK's SSE client relies on this). Since **0.1.111** the query-credential fallback is mounted ONLY on the four realtime routes — `authenticate_api_key_allow_query` in `src/api/middleware/auth.rs`, wired to its own router in `src/api/mod.rs`. All other protected routes use `authenticate_api_key`, which is header-only, closing the Referer/log leak on the general REST surface.
