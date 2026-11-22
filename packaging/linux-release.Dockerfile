# Build with the Ubuntu 22.04 linker and libc while running on a supported CI host.
FROM ubuntu:22.04@sha256:f64b2ffc2f10ab8f8505aa56b2664bd846cbb9df87c7e2532e30f7667021c0e6

RUN apt-get update \
    && apt-get install --yes --no-install-recommends \
        build-essential ca-certificates cmake git perl pkg-config \
    && rm -rf /var/lib/apt/lists/*

ENV PATH="/opt/rust/bin:$PATH"
WORKDIR /workspace
