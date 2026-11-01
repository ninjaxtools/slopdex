# Portable slopdex build: glibc 2.35 / gcc 11, matching the ubuntu-22.04 release runner.
FROM ubuntu:22.04
ARG RUST_VERSION=1.90.0
ENV DEBIAN_FRONTEND=noninteractive RUSTUP_HOME=/opt/rustup PATH=/opt/cargo/bin:$PATH
RUN apt-get update && apt-get install -y --no-install-recommends build-essential ca-certificates curl pkg-config git \
 && rm -rf /var/lib/apt/lists/*
RUN curl -sSf https://sh.rustup.rs | CARGO_HOME=/opt/cargo sh -s -- -y --profile minimal --default-toolchain "$RUST_VERSION" \
 && chmod -R a+rX /opt/rustup /opt/cargo
