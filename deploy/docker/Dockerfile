# Minimodes servers on Alpine (musl): builds lobby, gamehost and mmbot, then
# one slim runtime image per binary (`--target lobby|gamehost|mmbot`).
# Context is the repository root. Works with Docker/BuildKit and Podman/Buildah.
#
# Release builds use fat LTO and one codegen unit like production, which is
# slow. For a quicker build: --build-arg LTO=thin --build-arg CGU=16

ARG RUST_IMAGE=docker.io/library/rust:1.97.1-alpine3.24
ARG ALPINE_IMAGE=docker.io/library/alpine:3.24

FROM ${RUST_IMAGE} AS build
# g++ for Luau and the other -sys crates; git for ChunkEdge (a git dependency).
RUN apk add --no-cache musl-dev g++ make cmake perl git linux-headers pkgconf
ARG LTO=fat
ARG CGU=1
# The alpine toolchain defaults to a static C runtime; the C++ crates want the
# shared one (libstdc++ and libgcc are installed in the runtime image).
ENV RUSTFLAGS="-C target-feature=-crt-static" \
    CARGO_PROFILE_RELEASE_LTO=${LTO} \
    CARGO_PROFILE_RELEASE_CODEGEN_UNITS=${CGU}
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p lobby -p gamehost -p mmbot \
    && mkdir -p /out \
    && cp target/release/lobby target/release/gamehost target/release/mmbot /out/

FROM ${ALPINE_IMAGE} AS runtime
RUN apk add --no-cache libstdc++ libgcc ca-certificates \
    && addgroup -S -g 10001 minimodes \
    && adduser -S -u 10001 -G minimodes -h /var/lib/minimodes minimodes \
    && install -d -o minimodes -g minimodes /var/lib/minimodes/worlds
USER minimodes
WORKDIR /var/lib/minimodes
ENV RUST_LOG=info

FROM runtime AS lobby
COPY --from=build /out/lobby /usr/local/bin/lobby
ENTRYPOINT ["lobby"]
CMD ["/etc/minimodes/lobby.toml"]

FROM runtime AS gamehost
COPY --from=build /out/gamehost /usr/local/bin/gamehost
ENTRYPOINT ["gamehost"]
CMD ["/etc/minimodes/gamehost.toml"]

FROM runtime AS mmbot
COPY --from=build /out/mmbot /usr/local/bin/mmbot
ENTRYPOINT ["mmbot"]
