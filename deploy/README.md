# Production host (OCI Ampere A1, aarch64)

`scripts/build-release.sh` cross-compiles the binaries into `dist/bin/`.
`scripts/deploy.sh user@host` installs these and restarts the network:

- binaries, plus the backup scripts, go to `/opt/minimodes/bin/`;
- configs go to `/etc/minimodes/`;
- systemd units, including the nightly backup timer, are installed.

Everything below is one-time setup on the host.

## Base

```sh
sudo useradd --system --home /var/lib/minimodes --create-home minimodes
sudo install -d -o minimodes -g minimodes /var/lib/minimodes/worlds
sudo install -m 0600 -o minimodes deploy/minimodes.env.example /etc/minimodes/minimodes.env  # fill in
```

Put Gate's `linux_arm64` release at `/opt/minimodes/bin/gate`. Open TCP
25565 (Gate), and 443 and 80 (Caddy) in the OCI security list and on the
host firewall. Every other port listens on loopback only.

## Postgres

Create a role and database named `minimodes`, owned by that role, reached
over the local socket with peer auth. The processes run migrations on
start. For a 24 GB box shared with the game servers, use:

```
shared_buffers = 512MB
max_connections = 40
```

## Caddy (the script editor over HTTPS)

1. Install Caddy.
2. Copy `deploy/caddy/Caddyfile` to `/etc/caddy/Caddyfile`, replacing
   `edit.example.net` with your hostname.
3. Set the same origin as `editor_url` in `gamehost-*.toml`.

Caddy proxies to the lobby's `[web]` on 127.0.0.1:8081. `/metrics` isn't
served there, so nothing else is public.

## Ranks and the first admin

Ranks and their limits are the `[ranks.*]` tables in `lobby.toml`. Join
once, then:

```sh
sudo -u minimodes env $(cat /etc/minimodes/minimodes.env | xargs) \
  /opt/minimodes/bin/lobby /etc/minimodes/lobby.toml rank set <you> admin
```

Admins grant ranks in game with `/rank set <player> <rank>`. The moderator
tools (`/mod`, `/creatorban`) are covered in `config/dev/README.md`.

## Backups

`minimodes-backup.timer` runs `minimodes-backup.sh` nightly at 04:15 UTC. It
writes a `pg_dump` and syncs `worlds/`, which holds content-addressed `.mmw`
maps, to OCI Object Storage with rclone. Each piece is kept for 14 days.

1. Install `rclone` and the Postgres client (`pg_dump`).
2. Create a bucket (for example `minimodes-backups`) and a Customer Secret
   Key for S3 compatibility.
3. Write `/etc/minimodes/rclone.conf`, readable by `minimodes` only:

```ini
[oci]
type = s3
provider = Other
access_key_id = <key id>
secret_access_key = <secret>
endpoint = https://<namespace>.compat.objectstorage.<region>.oraclecloud.com
region = <region>
```

`MINIMODES_BACKUP_REMOTE` (default `oci:minimodes-backups`) and
`MINIMODES_BACKUP_KEEP` (default `14d`) can go in `minimodes.env`.

To run one now: `sudo systemctl start minimodes-backup.service`, then
`journalctl -u minimodes-backup`.

**Restore** (it replaces the database):

```sh
sudo systemctl stop minimodes.target
sudo -u minimodes /opt/minimodes/bin/minimodes-restore.sh 2026-11-07
sudo systemctl start minimodes.target
```

Check that restores work every so often, ideally onto a scratch database:
set `MINIMODES_BACKUP_DATABASE` and `MINIMODES_DATA_DIR`.

## Metrics

Each process serves Prometheus text at `metrics_bind`: the lobby on :9101,
and the shards on :9111 and :9112.

1. Install Prometheus, plus node_exporter on :9100 if you want host
   metrics.
2. Copy these files and start Prometheus with
   `--storage.tsdb.retention.time=30d`:
   - `deploy/prometheus/prometheus.yml` to `/etc/prometheus/prometheus.yml`;
   - `minimodes-alerts.yml` next to it.
3. Install Grafana bound to loopback, and reach it through an SSH tunnel or
   a second Caddy site with auth.
4. Copy `deploy/grafana/provisioning/*` into `/etc/grafana/provisioning/`.
5. Copy `deploy/grafana/dashboards/minimodes.json` to
   `/var/lib/grafana/dashboards/minimodes/`.

The **Minimodes** dashboard shows:

- players, shards, games and the moderation backlog;
- tick p99 per process;
- the instances with the slowest script steps and the most script memory;
- DataStore traffic and the script text log rate.

The alerts fire when:

- a process is down;
- a shard is missing;
- ticks are slow;
- held games have waited over 12 hours.

A dead shard is safe to check with `systemctl kill minimodes-gamehost@1`.
The lobby stays up, and `mm_shards_connected` drops until systemd
restarts the shard.
