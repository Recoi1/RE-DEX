# The central RE:DEX servers: the account backend and the central world (no Steam). Multi-arch: build it on the target
# machine (an ARM64 box builds linux/arm64 natively) or with buildx for both:
#   docker buildx build --platform linux/amd64,linux/arm64 -f deploy/central/Dockerfile --target backend .
# Build context: the repository root. Base images are pinned by digest (multi-arch index digests): bump them on purpose.

FROM rust:1-bookworm@sha256:1b9dc70890ec494e466c87f5ad41ad544bfb30ea6acc99a3e068c47a4ae3f23e AS build
WORKDIR /src
COPY . .
# --locked: exactly Cargo.lock's crates; the world without default features never pulls in steamworks
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p redex-backend \
 && cargo build --release --locked -p redex-world --no-default-features --features central \
 && mkdir -p /out \
 && cp target/release/redex-backend target/release/redex-world /out/

FROM debian:bookworm-slim@sha256:e67eeead736ff7bbe258ef088bff683635c561b1fcfefad0806c1df4ab49ce0e AS runtime
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --home-dir /data --create-home redex
COPY --from=build /out/ /usr/local/bin/
USER redex
WORKDIR /data

FROM runtime AS backend
ENV REDEX_BIND=0.0.0.0:8080 \
    REDEX_LOG_FORMAT=json
EXPOSE 8080
# bash's /dev/tcp: no curl in the image
HEALTHCHECK --interval=30s --timeout=5s --start-period=60s \
  CMD bash -c 'exec 3<>/dev/tcp/127.0.0.1/8080 && printf "GET /healthz HTTP/1.0\r\n\r\n" >&3 && grep -q " 200 " <&3'
CMD ["redex-backend"]

FROM runtime AS world
EXPOSE 7777
CMD ["redex-world", "--central", "--bind", "0.0.0.0:7777", "--data", "/data"]
