# Dataflow: Auth / Tenant

## REST (`api/middleware/auth.rs`)

1. Read `Authorization: Bearer …` — **header only** (`authenticate_api_key`). Since **0.1.111** `?token=` / `?api_key=` on a general REST route is a `401`; the query fallback lives on a separate realtime router (see Realtime).
2. If token starts with `eyJ` → JWT path (signature, blacklist, **re-check** active/expiry/queues from DB).
3. Else → API key (`sp_` / legacy `sk_`): `lookup_hash` + bcrypt verify.
4. Context carries `organization_id` + queue scope (`can_access_queue`).

## Admin (`api/middleware/admin_auth.rs`)

`X-Admin-Key` compared to configured `ADMIN_API_KEY` (SHA-256 then constant-time compare).

## API key bookkeeping

`api_keys.last_used` is written at most **once per key per 5 minutes**, not once per request (`touch_last_used` + Redis write guard, `src/api/middleware/auth.rs` ~530–575). Deliberate write-amplification cap. Treat it as "was active in this 5-minute bucket", never as a live request timestamp.

## gRPC (`grpc/auth.rs`)

`x-api-key` or `authorization: Bearer` — **API-key path only** (no JWT). Soft-deleted orgs (`plan_tier <> 'deleted'`) excluded.

## Scoping

Handlers bind org from auth context; `require_queue_access` for queue ACL. Isolation is application-level `WHERE organization_id = …`. RLS policies in DB are **inert** (see architecture guide) — do not rely on them as a backstop.

## Realtime

WS typically JWT via `?token=`; SSE may accept `?api_key=` (the Go SDK's SSE client relies on this). Since **0.1.111** the query-credential fallback is mounted ONLY on the four realtime routes — `authenticate_api_key_allow_query` in `src/api/middleware/auth.rs`, wired to its own router in `src/api/mod.rs`. All other protected routes use `authenticate_api_key`, which is header-only, closing the Referer/log leak on the general REST surface.
