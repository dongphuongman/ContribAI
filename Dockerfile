FROM rust:1.88-bookworm@sha256:af306cfa71d987911a781c37b59d7d67d934f49684058f96cf72079c3626bfe0 AS builder

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates/contribai-rs/Cargo.toml crates/contribai-rs/Cargo.toml
COPY crates/contribai-rs/src crates/contribai-rs/src
COPY crates/contribai-rs/benches crates/contribai-rs/benches

RUN cargo build --workspace --release --locked

FROM debian:bookworm-slim@sha256:abd67ffcfa541b485a3dff59865ab629aa048a6c613e639d36e7456b0b229241 AS runtime

LABEL org.opencontainers.image.title="ContribAI"
LABEL org.opencontainers.image.description="Maintainer-governed AI contribution admission and evidence"
LABEL org.opencontainers.image.source="https://github.com/tang-vu/ContribAI"
LABEL org.opencontainers.image.licenses="AGPL-3.0-or-later"

RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 10001 --shell /usr/sbin/nologin contribai

COPY --from=builder /src/target/release/contribai /usr/local/bin/contribai

USER 10001:10001
WORKDIR /home/contribai
VOLUME ["/home/contribai/.contribai"]
EXPOSE 8787
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl --fail --silent http://127.0.0.1:8787/api/health || exit 1

ENTRYPOINT ["contribai"]
CMD ["--help"]
