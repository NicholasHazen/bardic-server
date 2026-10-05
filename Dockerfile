# syntax=docker/dockerfile:1

FROM rust:1.93.1-slim-bookworm@sha256:5b9332190bb3b9ece73b810cd1f1e9f06343b294ce184bcb067f0747d7d333ea AS source
WORKDIR /app
# Leave room for other Spark workloads; callers can choose a different build limit.
ARG CARGO_BUILD_JOBS=4
ENV CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS}
RUN apt-get update \
    && apt-get install -y --no-install-recommends build-essential ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY crates/bardic-server/Cargo.toml crates/bardic-server/Cargo.toml
COPY crates/bardic-server/src/ crates/bardic-server/src/
COPY crates/bardic-server/migrations/ crates/bardic-server/migrations/

# Explicit deployment gate, including every integration test and the normative
# contract used by their conformance harness. Live provider tests stay ignored.
FROM source AS verify
RUN apt-get update \
    && apt-get install -y --no-install-recommends ffmpeg procps \
    && rm -rf /var/lib/apt/lists/* \
    && rustup component add rustfmt clippy
COPY crates/bardic-server/tests/ crates/bardic-server/tests/
COPY docs/contract/ docs/contract/
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/app/target \
    cargo fmt --check \
    && cargo clippy --locked --all-targets -- -D warnings \
    && cargo test --locked

FROM source AS build
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/app/target \
    cargo build --locked --release -p bardic-server \
    && install -D target/release/bardic-server /out/bardic-server

FROM debian:bookworm-slim@sha256:3783cc01769c7b2b1b83a5c5ad96c815348e28ed7da68e2e3687004faa906251 AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates ffmpeg curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 bardic \
    && useradd --uid 10001 --gid bardic --no-create-home --shell /usr/sbin/nologin bardic \
    && install -d -m 0700 -o 10001 -g 10001 /data
COPY --from=build /out/bardic-server /usr/local/bin/bardic-server
USER 10001:10001
EXPOSE 8765
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/bardic-server"]
CMD ["--data-dir", "/data", "--bind", "0.0.0.0:8765"]
