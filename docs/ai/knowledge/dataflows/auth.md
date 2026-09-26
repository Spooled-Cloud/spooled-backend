# Dataflow: Auth / Tenant

## REST (`api/middleware/auth.rs`)

1. Read `Authorization: Bearer …` — **header only** (`authenticate_api_key`). Since **0.1.111** `?token=` / `?api_key=` on a general REST route is a `401`; the query fallback lives on a separate realtime router (see Realtime).
2. If token starts with `eyJ` → JWT path (signature, blacklist, **re-check** active/expiry/queues from DB).
3. Else → API key (`sp_` / legacy `sk_`): `lookup_hash` + bcrypt verify.
4. Context carries `organization_id` + queue scope (`can_access_queue`).

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

## Billing email uniqueness

Email login resolves an org by lower-cased `billing_email`, so every path that sets
one (public create, admin create, both PATCH/PUT routes) rejects a duplicate among
live orgs with 409 (`organizations::billing_email_taken`). Before 0.1.114 admin
create/update accepted duplicates and the login landed in an arbitrary org.

## API key bookkeeping

`api_keys.last_used` is written at most **once per key per 5 minutes**, not once per request (`touch_last_used` + Redis write guard, `src/api/middleware/auth.rs` ~530–575). Deliberate write-amplification cap. Treat it as "was active in this 5-minute bucket", never as a live request timestamp.

## gRPC (`grpc/auth.rs`)

`x-api-key` or `authorization: Bearer` — **API-key path only** (no JWT). Soft-deleted orgs (`plan_tier <> 'deleted'`) excluded.

## Scoping

Handlers bind org from auth context; `require_queue_access` for queue ACL. Isolation is application-level `WHERE organization_id = …`. RLS policies in DB are **inert** (see architecture guide) — do not rely on them as a backstop.

## Realtime

`GET /api/v1/events` (global SSE) forwards the org's Redis pub/sub events (`org:{id}:events`,
the same channel the WebSocket reads) as named SSE events, plus `system.health` every 10 s;
it re-checks the key at most every 2 s and caps 100 streams per org. Before 0.1.114 it
only emitted `system.health`. Published today by REST job create/bulk/complete/fail/cancel/
retry only — gRPC, schedules, workflows and webhook ingest do not publish realtime events.

WS typically JWT via `?token=`; SSE may accept `?api_key=` (the Go SDK's SSE client relies on this). Since **0.1.111** the query-credential fallback is mounted ONLY on the four realtime routes — `authenticate_api_key_allow_query` in `src/api/middleware/auth.rs`, wired to its own router in `src/api/mod.rs`. All other protected routes use `authenticate_api_key`, which is header-only, closing the Referer/log leak on the general REST surface.
