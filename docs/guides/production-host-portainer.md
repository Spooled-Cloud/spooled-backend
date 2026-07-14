# Deploying Spooled backend with Portainer

Public operator guide for self-hosters using [Portainer](https://www.portainer.io/) with the production Compose file.

## Recommended setup

1. Create a **Git** stack (not Web editor) from this repository.
2. Compose path: `docker-compose.prod.yml` (repo root).
3. Branch: `main` (or the release branch you track).
4. Set required secrets in the **stack environment** (see compose file header / `.env.example`). Do not commit real secrets.
5. Image: leave unset or set  
   `BACKEND_IMAGE=ghcr.io/spooled-cloud/spooled-backend:latest`  
   The compose default is `:latest` with `pull_policy: always` so **Pull and redeploy** picks up new CI builds. Use a version tag or digest only for temporary rollback.
6. Deploy, then use **Pull and redeploy** for updates (same as other Git stacks).

Git stacks in Portainer CE show **Git configuration** + **Pull and redeploy**, not the Web **Editor** tab. That is normal.

## Zero-touch init

`docker-compose.prod.yml` is self-contained (single-file download works):

| Concern | Behavior |
|---------|----------|
| gRPC origin TLS | Helper writes/renews certs into volume `grpc_tls`, then stays **Up/healthy**. For CI one-shot checks set `GRPC_TLS_INIT_ONCE=1`. |
| Prometheus | Writes scrape config on start, then runs. |
| Grafana | Writes datasource provisioning on start, then runs. |

No busybox one-shot “Exited (0)” init containers are required.

## Portainer Git working directories

Portainer clones Git stacks under its data directory (path varies by install), often including a commit SHA segment. After **Pull and redeploy**, some installs empty or remove that checkout while containers keep running. If the UI then says it cannot retrieve `docker-compose.prod.yml`:

- Confirm containers are still healthy (`docker ps`, public `/health`, authenticated dashboard if you use one).
- Point the stack at a **stable host directory** you control (bind/relative path or Web editor paste), **or** recreate the Git stack carefully (see below).
- Prefer keeping secrets only in Portainer stack env (or a host `.env` outside Git), never in the public repo.

Do not publish host-specific SSH targets, private IPs, or internal stack IDs in shared docs.

## Safe recreate

If the Git stack metadata is corrupt:

1. Confirm named volumes for your project still exist.
2. Delete the stack with **Remove volumes = OFF** (volumes on destroys Postgres/Redis data).
3. Recreate with the **same stack/project name** so volume names reuse.
4. Restore env vars; image `:latest` unless rolling back.
5. Verify health and your application endpoints.

## Isolation on shared Docker hosts

Always pass an explicit Compose project name (this file expects operators to use a dedicated project such as `spooled-backend`). Never run `docker compose down -v` against a production data project unless you intend to wipe volumes. Do not stop or remove containers belonging to unrelated stacks on a shared machine.

## Name conflict

```
Conflict. The container name "/spooled_backend" is already in use
```

Something owns that name outside the Compose project (often a one-off `docker run`). Adopt or remove **only** that container, then redeploy the stack. Never mass-delete unrelated containers.

## Related

- [deployment.md](./deployment.md) — broader deploy options
- [operations.md](./operations.md) — day-2 ops
- Backend README — quick start + Portainer summary
