# Dataflow: Auth / Tenant

## REST (`api/middleware/auth.rs`)

1. Read `Authorization: Bearer …` or query `token` / `api_key`.
2. If token starts with `eyJ` → JWT path (signature, blacklist, **re-check** active/expiry/queues from DB).
3. Else → API key (`sp_` / legacy `sk_`): `lookup_hash` + bcrypt verify.
4. Context carries `organization_id` + queue scope (`can_access_queue`).

## Admin (`api/middleware/admin_auth.rs`)

`X-Admin-Key` compared to configured `ADMIN_API_KEY` (SHA-256 then constant-time compare).

## gRPC (`grpc/auth.rs`)

`x-api-key` or `authorization: Bearer` — **API-key path only** (no JWT). Soft-deleted orgs (`plan_tier <> 'deleted'`) excluded.

## Scoping

Handlers bind org from auth context; `require_queue_access` for queue ACL. Isolation is application-level `WHERE organization_id = …`. RLS policies in DB are **inert** (see architecture guide) — do not rely on them as a backstop.

## Realtime

WS typically JWT via `?token=`; SSE may accept `?api_key=`. Query-token fallback on general protected REST routes remains a Referer/log leak risk (P3).
