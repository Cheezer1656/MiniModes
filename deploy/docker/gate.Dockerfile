# Gate (minekube) from its release binary, static and musl. Context is unused.
ARG ALPINE_IMAGE=docker.io/library/alpine:3.24

FROM ${ALPINE_IMAGE} AS fetch
RUN apk add --no-cache curl
ARG TARGETARCH
ARG GATE_VERSION=0.76.3
ARG GATE_SHA256_AMD64=b819a2f58a99c42253847991de9ccfc8c7b20add101a486f7afc3ab4bd2994ee
ARG GATE_SHA256_ARM64=64baa5b07014f3250ac0345cc073fb55a6065926d86e06109822cc6d285e533f
RUN case "${TARGETARCH:-amd64}" in \
      amd64) sha="${GATE_SHA256_AMD64}" ;; \
      arm64) sha="${GATE_SHA256_ARM64}" ;; \
      *) echo "no Gate build for ${TARGETARCH}" >&2; exit 1 ;; \
    esac \
    && curl -fsSL -o /gate "https://github.com/minekube/gate/releases/download/v${GATE_VERSION}/gate_${GATE_VERSION}_linux_${TARGETARCH:-amd64}_musl" \
    && echo "${sha}  /gate" | sha256sum -c - \
    && chmod 0755 /gate

FROM ${ALPINE_IMAGE}
RUN apk add --no-cache ca-certificates \
    && addgroup -S -g 10001 gate && adduser -S -u 10001 -G gate -h /home/gate gate
COPY --from=fetch /gate /usr/local/bin/gate
USER gate
WORKDIR /home/gate
# GATE_VELOCITY_SECRET comes from the environment.
ENV GOMAXPROCS=1
ENTRYPOINT ["gate"]
CMD ["--config", "/etc/gate/gate.yml", "--no-auto-reload"]
