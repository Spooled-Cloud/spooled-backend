# Production host + Portainer (Spooled backend)

This document is the operator guide for the live Spooled **backend** stack on the shared production host. It exists because Portainer Git deploys used ephemeral directories under `/data/compose/71/<git-sha>/`, which disappear and then break the UI (`Could not get the contents of the file 'docker-compose.prod.yml'`) and collide with manually recreated containers (`Conflict … spooled_backend already in use`).

## Durable layout (source of truth)

| Item | Value |
|------|--------|
| Directory | `/opt/spooled/backend` |
| Compose file | `docker-compose.prod.yml` |
| Secrets | `.env` (mode `600`, never commit) |
| Image pin | `.env.image` with `BACKEND_IMAGE=ghcr.io/spooled-cloud/spooled-backend:vX.Y.Z@sha256:…` |
| Compose project name | **`spooled-backend`** (always pass `-p spooled-backend`) |
| Safe CLI | `sudo /opt/spooled/backend/bin/compose …` |
| Redeploy API container | `sudo /opt/spooled/backend/bin/deploy-backend` |
| Stable symlink | `/data/compose/71/spooled-backend` → `/opt/spooled/backend` |

Dashboard is separate: `/opt/spooled/dashboard`, project `spooled-dashboard`. SpriteForge is separate: project `spooled-example-spriteforge`. Do not mix them.

## Host isolation (mandatory)

The production host runs **other** Compose projects (examples observed: `authentik`, `outlinewiki`, `clipcascade`, `keymano`, `opn-onl`, `soketi-daresinxyz`, plus other Spooled apps). Spooled backend work must never touch them.

**Allowed for backend ops**

- Compose project: `spooled-backend` only
- Working directory: `/opt/spooled/backend` only (or its symlink `/data/compose/71/spooled-backend`)
- Containers owned by that project (names like `spooled_backend`, `spooled_db`, `spooled_redis`, `spooled_cloudflared`, `spooled_prometheus`, `spooled_grafana`, `spooled_grpc_tls_init`)

**Forbidden**

- `docker compose` without `-p spooled-backend` from a random directory
- `docker compose down -v` / `--volumes` (destroys DB/Redis data)
- `docker stop $(docker ps -q)` or any host-wide stop/rm
- Editing or redeploying another stack’s tree under `/data/compose/<other-id>/`
- Changing `spooled-dashboard` or `spooled-example-spriteforge` while doing backend work

The wrappers under `/opt/spooled/backend/bin/` hard-code the project name and refuse `down -v`.

## Prefer CLI over Portainer for rollouts

Portainer on this host is an **agent only**; the UI is remote. Git “Pull and redeploy” recreates ephemeral `/data/compose/71/<sha>/` dirs and is what caused the missing-file / name-conflict errors.

**Recommended rollout (backend image only):**

```bash
ssh opc@<prod-host>
# Edit image pin (digest from GHCR for the release tag)
sudoedit /opt/spooled/backend/.env.image
# Example:
# BACKEND_IMAGE=ghcr.io/spooled-cloud/spooled-backend:v0.1.102@sha256:…

sudo /opt/spooled/backend/bin/deploy-backend
```

Verify (not `/health` alone for version proof):

```bash
curl -sS https://api.spooled.cloud/health
# Then authenticated GET /api/v1/dashboard → system.version + fresh uptime_seconds
```

## How to fix Portainer so the UI matches reality

Goal: Portainer must load compose from the **durable** directory, not a deleted Git sha folder.

### Option A — Web editor (works on Portainer CE)

1. Open the environment that has **Portainer Agent** on this host.
2. Go to **Stacks** → open the stack named like **`spooled-backend`** (not dashboard / spriteforge / other apps).
3. Click **Editor**.
4. If you see `Unable to retrieve stack file…`, the old path is dead. Switch build method to **Web editor** (or re-open Editor after the durable symlink heal — see below).
5. Paste the contents of `/opt/spooled/backend/docker-compose.prod.yml` from the host (copy via SSH; do not invent a second compose).
6. In stack env vars, align with `/opt/spooled/backend/.env` + set `BACKEND_IMAGE` to the digest in `.env.image`. Prefer pinning `@sha256:…`, not floating `:latest`.
7. Confirm the stack **name** / project remains `spooled-backend`.
8. Deploy / update **once**. Do not use Git pull-and-redeploy afterward unless the stack is re-pointed.

### Option B — Relative / bind path (if your Portainer edition supports it)

1. Stack settings → use host path **`/opt/spooled/backend`** (or `/data/compose/71/spooled-backend`).
2. Compose file: `docker-compose.prod.yml`.
3. Save. Future updates should read the durable file.

### Option C — Stop managing this stack in Portainer

Leave Portainer as read-only visibility. All changes go through `/opt/spooled/backend/bin/*` only. This is valid and often safer on a multi-tenant host.

## Symlink heal (already applied on host)

If Portainer still asks for a deleted sha directory, these links may exist so the Editor can read files without touching other stacks:

- `/data/compose/71/spooled-backend` → `/opt/spooled/backend`
- Optional heal links for previously deleted sha dirs used only by this stack → same target

Do **not** create symlinks for other numeric stack IDs under `/data/compose/` unless you are sure they are Spooled backend.

## Name-conflict error

```
Conflict. The container name "/spooled_backend" is already in use
```

Means something (often a manual `docker run`) owns `spooled_backend` outside the compose project. Fix by adopting into the durable project (do not delete unrelated containers):

```bash
sudo /opt/spooled/backend/bin/compose up -d backend
```

If a true orphan blocks create, rename/remove **only** `spooled_backend`, then run the command above. Never `docker rm` other projects’ containers.

## Related docs

- [deployment.md](./deployment.md) — general deploy options + release checklist
- [operations.md](./operations.md) — day-2 ops
- Dashboard durable path: `spooled-dashboard/docs/DEPLOYMENT.md` (`/opt/spooled/dashboard`)
