# Production host + Portainer (Spooled backend)

Operator guide for the live Spooled **backend** stack on the shared production host.

## Goal

Portainer **Git → Pull and redeploy** (repull images) must work for `spooled-backend` the same way it does for other stacks. Day-to-day rollouts are UI clicks, not SSH.

SSH helpers under `/opt/spooled/backend/bin/` remain for disaster recovery and host heals only. The heal script is also checked in as [`scripts/heal-portainer-stack-files.sh`](../../scripts/heal-portainer-stack-files.sh).

**Newcomers / other hosts:** `docker-compose.prod.yml` is self-contained. gRPC TLS, Prometheus scrape config, and Grafana datasource provisioning bootstrap automatically — no busybox Exited (0) init containers and no extra files beyond the compose + `.env`. See [deployment.md](./deployment.md#zero-touch-init-self-host--portainer--single-file).

## Why Editor is missing

This stack is a **Git** stack. Portainer CE Git stacks show **Git configuration** + **Pull and redeploy**, not the Web **Editor** tab. That is normal. Other stacks that use Web editor / upload show Editor; Git stacks do not.

If you need an Editor tab, you must retire the Git stack and recreate it as **Web editor** (see recreate section). Prefer keeping Git + Pull and redeploy.

## Layout

| Item | Value |
|------|--------|
| Portainer stack name / Compose project | **`spooled-backend`** |
| Live WD (preferred) | `/opt/spooled/backend` (durable) — Portainer Git sha dirs often get wiped after deploy |
| Portainer stack id (this host) | `71` → `/data/compose/71/<git-sha>/` (may be symlink → durable) |
| Compose file in repo | `docker-compose.prod.yml` (repo root) |
| Disaster-recovery mirror | `/opt/spooled/backend` (compose, `.env` mode `600`, `.env.image`, `bin/*`) |
| Image pin | Stack env / `.env`: `BACKEND_IMAGE=ghcr.io/spooled-cloud/spooled-backend:vX.Y.Z@sha256:…` |

Dashboard is separate: `/opt/spooled/dashboard`, project `spooled-dashboard`. SpriteForge is separate. Do not mix them.

## Host isolation (mandatory)

The production host runs **other** Compose projects (`authentik`, `outlinewiki`, `clipcascade`, `keymano`, `opn-onl`, `soketi-daresinxyz`, plus other Spooled apps). Backend work must never touch them.

**Allowed**

- Compose project: `spooled-backend` only
- Paths: `/data/compose/71/**` and `/opt/spooled/backend/**`
- Containers owned by that project (`spooled_backend`, `spooled_db`, `spooled_redis`, …)

**Forbidden**

- `docker compose` without `-p spooled-backend` from a random directory
- `docker compose down -v` / **Remove volumes** in Portainer (destroys DB/Redis)
- Host-wide `docker stop` / `docker rm`
- Editing `/data/compose/<other-id>/`
- Changing dashboard or SpriteForge while doing backend work

## Normal rollout (preferred): Portainer Pull and redeploy

1. Open Portainer → environment with this host’s agent → **Stacks** → **`spooled-backend`**.
2. Confirm Git repo is `https://github.com/Spooled-Cloud/spooled-backend.git`, compose path `docker-compose.prod.yml`, branch `main` (or the branch you intend).
3. Confirm stack env includes a pinned `BACKEND_IMAGE=…@sha256:…` (update the pin when promoting a new release). Prefer digest over floating `:latest`.
4. Click **Pull and redeploy** (repull images).
5. Wait until containers are healthy.
6. Verify:
   - Public `GET https://api.spooled.cloud/health` → 200
   - Authenticated `GET /api/v1/dashboard` → `system.version` + fresh `uptime_seconds` (not `/health` alone for version proof)

After a successful pull, containers should be healthy. Portainer may still leave the Git checkout under `/data/compose/71/<sha>/` **empty or deleted** (labels point at a missing path; UI cannot open the compose file). That does **not** mean the stack is down — heal immediately:

```bash
ssh opc@<prod-host>
sudo /opt/spooled/backend/bin/heal-portainer-stack-files
# If containers still label a missing WD, re-adopt from durable:
cd /opt/spooled/backend && sudo ./bin/compose up -d
```

Also pin `BACKEND_IMAGE` in the Portainer stack env (digest). Without it, Pull and redeploy often floats to `:latest`.

### If UI says it cannot retrieve `docker-compose.prod.yml`

Same heal as above (stack id `71` only):

```bash
ssh opc@<prod-host>
sudo /opt/spooled/backend/bin/heal-portainer-stack-files
```

Then refresh Portainer. Do not run heals against other stack IDs.

## Safe recreate (only if Pull and redeploy stays broken)

Use this when the Git stack metadata is corrupt and you want a clean Portainer stack with the same volumes.

**Warning:** In Portainer delete dialog, **do not** check **Remove volumes**. Checking it destroys Postgres/Redis data.

1. From the host, confirm volumes exist: `docker volume ls | grep '^spooled-backend_'`.
2. In Portainer → **Stacks** → **`spooled-backend`** → **Delete** with **Remove volumes = OFF**.
3. Create stack again:
   - Name: **`spooled-backend`** (must match so volume names reuse)
   - Method: **Repository**
   - URL: `https://github.com/Spooled-Cloud/spooled-backend.git`
   - Compose path: `docker-compose.prod.yml`
   - Branch: `main`
   - Env vars: copy from host `/opt/spooled/backend/.env` (and set `BACKEND_IMAGE` digest). Never commit or paste secrets into chat.
4. Deploy. Compose reuses existing `spooled-backend_*` volumes when the project name matches.
5. Verify health + authenticated dashboard version as above.

## Disaster-recovery CLI (SSH fallback)

```bash
ssh opc@<prod-host>
sudoedit /opt/spooled/backend/.env.image   # pin BACKEND_IMAGE digest
sudo /opt/spooled/backend/bin/deploy-backend
```

Wrappers hard-code `-p spooled-backend` and refuse `down -v`.

## Name-conflict error

```
Conflict. The container name "/spooled_backend" is already in use
```

Something owns `spooled_backend` outside the compose project (often a one-off `docker run`). Adopt back into the project from the current Portainer checkout or durable mirror; never remove unrelated containers.

## Related docs

- [deployment.md](./deployment.md) — general deploy options + release checklist
- [operations.md](./operations.md) — day-2 ops
- Dashboard durable path: `spooled-dashboard/docs/DEPLOYMENT.md` (`/opt/spooled/dashboard`)
