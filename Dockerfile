# syntax=docker/dockerfile:1

FROM rust:1.88-bullseye AS build
ARG TARGETARCH
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=cache,id=cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=cargo-git,target=/usr/local/cargo/git \
    --mount=type=cache,id=redmine-target-${TARGETARCH},target=/src/target \
    cargo test --locked --release \
 && cargo build --locked --release \
 && mkdir -p /out \
 && cp /src/target/release/redmine-dev-mcp /out/redmine-dev-mcp

FROM scratch AS bin
COPY --from=build /out/redmine-dev-mcp /redmine-dev-mcp
