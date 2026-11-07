# Container stack for end-to-end tests

Everything in `deploy/` running together on Alpine, under Docker or Podman:

| Container | Image | What it tests |
| --- | --- | --- |
| `mm-postgres` | `postgres:18-alpine` | migrations, `max_connections=40` like production |
| `mm-gate` | Gate 0.76.3 (musl release, sha256-checked) | real Velocity forwarding and the management API |
| `mm-lobby`, `mm-gamehost-1/2` | built from this repo | the servers, with two shards registering themselves |
| `mm-caddy` | `caddy:alpine` | the production Caddyfile (no TLS) in front of the web editor |
| `mm-prometheus`, `mm-grafana` | upstream images | scrape targets, alert rules, dashboard provisioning |
| `mm-objectstore`, `mm-backup` | built here (`rclone`, `postgresql-client`) | the backup and restore scripts against an S3 stand-in |
| `mm-bot` | built from this repo | `mmbot`, run inside the network |

Prometheus is the one image that isn't Alpine (upstream publishes none).

## Run

```sh
./e2e.sh              # build, start a fresh stack, run every check
./e2e.sh --no-build   # reuse the images
./e2e.sh --down       # remove the containers and volumes afterwards
```

`ENGINE=docker|podman` and `COMPOSE="docker compose"` override the detection
(the first working engine wins). The checks, in order:

1. all containers healthy, shards registered with Gate;
2. `mmbot join` and `lobby-ui` **through Gate**;
3. `mmbot gate`: create and publish a game, then lobby → shard → lobby on one
   connection (Gate does the server switches);
4. an editor sign-in link through Caddy (HSTS set, `/metrics` not exposed);
5. Prometheus targets and alert rules, Grafana's dashboard and datasource;
6. `minimodes-backup.sh`, then wipe `games` and a world file, stop the
   servers and `minimodes-restore.sh`;
7. servers restarted on the restored data, and a shard stopped and started.

The stack is left running for poking: Gate on `localhost:25565` (offline mode,
any username), the editor on `:8080`, Prometheus on `:9090`, Grafana on `:3000`
(admin/admin). Override with `MM_GATE_PORT`, `MM_EDITOR_PORT`,
`MM_PROMETHEUS_PORT`, `MM_GRAFANA_PORT`. `MM_SECRET` is the Velocity secret.

By hand: `docker compose up -d --build` or `podman compose up -d --build` from
this directory; `docker exec mm-bot mmbot join 10.77.0.20:25565 3 5`.

## Build time

The images use the production release profile (fat LTO, one codegen unit),
which is slow. For a quicker build:

```sh
docker compose build --build-arg LTO=thin --build-arg CGU=16
```

To check aarch64 (the production target, which also builds Luau's JIT), set
`DOCKER_DEFAULT_PLATFORM=linux/arm64` (Podman: `--platform linux/arm64`).
Under QEMU emulation the build takes a long time.

## Podman

- Rootless works. `podman compose` hands off to `docker-compose` (or
  `podman-compose`) and needs the API socket: `systemctl --user enable --now
  podman.socket`. `e2e.sh` starts it if it's missing.
- The Dockerfiles have no `HEALTHCHECK` (Podman's default OCI image format
  drops it); health lives in `compose.yml`.
- Image names are fully qualified, bind mounts have `:z` (SELinux), and host
  ports are above 1024.

## Differences from production

- **musl, not glibc:** the binaries here are Alpine builds, so this doesn't
  test the exact release artifacts (those are glibc, cross-built for aarch64).
- **Static IPs:** the servers' configs take socket addresses, not hostnames,
  so the compose network is pinned to `10.77.0.0/24` and each config names
  the container's address. `gate_api` and similar URLs use service names.
- **Gate is offline-mode** with its API open on the network, the lobby has
  no vanilla world (it generates a flat platform), Postgres uses trust auth,
  and `rclone serve s3` stands in for OCI Object Storage.
- Backups run with a `systemctl` shim (`is-active` always says "no"), so stop
  the servers before restoring, as `e2e.sh` does.
- node_exporter isn't part of the stack, so the `node` Prometheus job is left
  out.
