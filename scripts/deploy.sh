#!/usr/bin/env bash
# Pushes binaries, configs and units to the VPS and restarts the network.
# Usage: scripts/deploy.sh user@host
# First-time host setup: create the `minimodes` user, /var/lib/minimodes,
# /etc/minimodes/minimodes.env (from deploy/minimodes.env.example), and put
# the Gate linux_arm64 release binary at /opt/minimodes/bin/gate.
set -euo pipefail
cd "$(dirname "$0")/.."
HOST=${1:?usage: deploy.sh user@host}

rsync -az dist/bin/ "$HOST:/tmp/minimodes-bin/"
rsync -az deploy/gate/config.yml "$HOST:/tmp/minimodes-gate.yml"
rsync -az deploy/config/ "$HOST:/tmp/minimodes-config/"
rsync -az deploy/systemd/ "$HOST:/tmp/minimodes-systemd/"

ssh "$HOST" 'set -e
  sudo install -d -o root -g root /opt/minimodes/bin /etc/minimodes
  sudo install -m 0755 /tmp/minimodes-bin/* /opt/minimodes/bin/
  sudo install -m 0644 /tmp/minimodes-gate.yml /etc/minimodes/gate.yml
  sudo install -m 0644 /tmp/minimodes-config/* /etc/minimodes/
  sudo install -m 0644 /tmp/minimodes-systemd/* /etc/systemd/system/
  sudo systemctl daemon-reload
  sudo systemctl enable minimodes.target
  sudo systemctl restart minimodes.target'
