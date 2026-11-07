# Backup/restore tooling (the deploy/backup scripts) and, in the stack, an
# S3 stand-in for OCI Object Storage (`rclone serve s3`). Context: repo root.
ARG ALPINE_IMAGE=docker.io/library/alpine:3.24

FROM ${ALPINE_IMAGE}
RUN apk add --no-cache bash rclone postgresql18-client ca-certificates \
    && addgroup -S -g 10001 minimodes \
    && adduser -S -u 10001 -G minimodes -h /var/lib/minimodes minimodes \
    && install -d -o minimodes -g minimodes /var/lib/minimodes/worlds /var/lib/minimodes/backup /var/lib/minimodes/s3
# The restore script asks systemd whether the lobby runs; no systemd here.
COPY deploy/docker/systemctl-shim.sh /usr/local/bin/systemctl
COPY deploy/backup/minimodes-backup.sh deploy/backup/minimodes-restore.sh /opt/minimodes/bin/
RUN chmod 0755 /usr/local/bin/systemctl /opt/minimodes/bin/*.sh
ENV PATH=/opt/minimodes/bin:$PATH
USER minimodes
WORKDIR /var/lib/minimodes
