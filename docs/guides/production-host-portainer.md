# Production host + Portainer (Spooled backend)

Operator guide for the live Spooled **backend** stack on the shared production host.

## Goal

Day-to-day rollouts are Portainer UI clicks: **Pull and redeploy** (repull images), same as other stacks. SSH is only for disaster recovery and heals.

**Newcomers / any host:** `docker-compose.prod.yml` is self-contained. One compose file + `.env` is enough. gRPC TLS and Prometheus/Grafana bootstrap automatically. See [deployment.md](./deployment.md#zero-touch-init-self-host--portainer--single-file).

## Image policy: `:latest` (intentional)

This product’s Portainer workflow is **follow `latest`**.

| Setting | Value |
|---------|--------|
| Default in compose | `BACKEND_IMAGE=ghcr.io/spooled-cloud/spooled-backend:latest` |
| Host `/opt/spooled/backend/.env` + `.env.image` | `BACKEND_IMAGE=ghcr.io/spooled-cloud/spooled-backend:latest` |
| Portainer stack env | Use the same, or omit `BACKEND_IMAGE` (compose default is `:latest`) |
| `pull_policy` | `always` on the backend service — every Pull and redeploy re-pulls |

CI on `main` publishes multi-arch `latest`. Clicking **Pull and redeploy** is how you pick up new builds.

Optional: set `BACKEND_IMAGE` to a version tag or `@sha256:…` only for a temporary rollback. That is not the normal path.

## Why there is no Editor tab

This stack is a **Git** stack. Portainer CE Git stacks show **Git configuration** + **Pull and redeploy**, not Web **Editor**. Normal. Keep Git + Pull and redeploy.

## Layout

| Item | Value |
|------|--------|
| Portainer stack / Compose project | **`spooled-backend`** |
| Preferred live WD | `/opt/spooled/backend` (durable) |
| Portainer stack id (this host) | `71` → `/data/compose/71/<git-sha>/` (often wiped or symlinked → durable) |
| Compose file | `docker-compose.prod.yml` (repo root) |
| Secrets | `/opt/spooled/backend/.env` (mode `600`) |
| Image | `BACKEND_IMAGE=…:latest` (see above) |
| Heal script | `/opt/spooled/backend/bin/heal-portainer-stack-files` (repo: [`scripts/heal-portainer-stack-files.sh`](../../scripts/heal-portainer-stack-files.sh)) |

Dashboard: `/opt/spooled/dashboard`, project `spooled-dashboard`. SpriteForge separate. Do not mix.

## What “heal” means (and when you need it)

### The problem

Portainer Git stacks clone the repo into something like:

`/data/compose/71/<git-commit-sha>/`

On **Pull and redeploy**, Portainer:

1. Pulls a new commit into a **new** sha directory
2. Runs `docker compose up`
3. Often **deletes or empties** the old (and sometimes the new) checkout afterward

Containers keep running (env came from Portainer stack env / last compose). But Docker labels may still say:

`com.docker.compose.project.working_dir=/data/compose/71/<old-or-new-sha>`

If that path is **gone**, Portainer UI shows errors like **Unable to retrieve stack file: docker-compose.prod.yml**. Looks scary; API may still be healthy.

### What the heal script does

`heal-portainer-stack-files` (stack **71** / Spooled backend **only**):

1. If the running `spooled_backend` container labels a WD that **does not exist**, symlink that path → `/opt/spooled/backend`
2. For empty leftover sha dirs under `/data/compose/71/`, link `docker-compose.prod.yml` and `.env` to the durable copies
3. Ensure `/data/compose/71/spooled-backend` → durable

It does **not** touch other stacks (`authentik`, `outlinewiki`, dashboard, …). It does **not** delete volumes.

### When to run it

```bash
ssh opc@<prod-host>
sudo /opt/spooled/backend/bin/heal-portainer-stack-files
```

Run after Pull and redeploy if:

- Portainer cannot open/retrieve `docker-compose.prod.yml`
- `docker inspect spooled_backend` shows a `working_dir` that `ls` says is missing

If containers are unhealthy or WD is still wrong after heal:

```bash
cd /opt/spooled/backend
sudo ./bin/compose up -d
```

That re-adopts the project onto the durable directory (labels → `/opt/spooled/backend`).

## Host isolation (mandatory)

**Allowed:** project `spooled-backend` only; paths `/data/compose/71/**` and `/opt/spooled/backend/**`.

**Forbidden:** compose without `-p spooled-backend`; `down -v` / Remove volumes; host-wide stop/rm; other stack trees; changing dashboard/SpriteForge while doing backend work.

## Normal rollout: Pull and redeploy

1. Portainer → this host’s agent env → **Stacks** → **`spooled-backend`**
2. Git: `https://github.com/Spooled-Cloud/spooled-backend.git`, compose `docker-compose.prod.yml`, branch `main`
3. Stack env: secrets from durable `.env`; **`BACKEND_IMAGE=ghcr.io/spooled-cloud/spooled-backend:latest`** (or omit)
4. **Pull and redeploy**
5. Wait healthy
6. Verify:
   - `GET https://api.spooled.cloud/health` → 200
   - Authenticated `GET /api/v1/dashboard` → `system.version` + fresh `uptime_seconds`
7. If UI cannot retrieve compose → run heal (above)

## Zero-touch init (no scary Exited 0)

| Concern | Behavior |
|---------|----------|
| gRPC TLS | `grpc-tls-init` writes/renews certs into volume `grpc_tls`, stays **Up/healthy** (`tail -f`). CI uses `GRPC_TLS_INIT_ONCE=1` for one-shot exit. |
| Prometheus | Writes scrape config on start, then runs Prometheus. |
| Grafana | Writes datasource provisioning on start, then runs Grafana. |

No busybox one-shot containers. Single-file `curl` of compose still works.

## Safe recreate (only if Pull stays broken)

**Warning:** Delete stack with **Remove volumes = OFF**. Volumes on = wipe DB/Redis.

1. Confirm `docker volume ls | grep '^spooled-backend_'`
2. Delete stack `spooled-backend`, volumes off
3. Recreate Git stack, name exactly `spooled-backend`, compose `docker-compose.prod.yml`
4. Env from `/opt/spooled/backend/.env`; image `:latest`
5. Deploy; verify health + authenticated dashboard

## Disaster-recovery CLI

```bash
ssh opc@<prod-host>
# Image follows latest by default in .env / .env.image
sudo /opt/spooled/backend/bin/deploy-backend
```

Wrappers hard-code `-p spooled-backend` and refuse `down -v`.

## Name-conflict error

```
Conflict. The container name "/spooled_backend" is already in use
```

Adopt back with durable compose (`./bin/compose up -d`). Never remove unrelated containers.

## Related docs

- [deployment.md](./deployment.md) — deploy options + zero-touch
- [operations.md](./operations.md) — day-2 ops
- Dashboard: `spooled-dashboard/docs/DEPLOYMENT.md` (`/opt/spooled/dashboard`)
