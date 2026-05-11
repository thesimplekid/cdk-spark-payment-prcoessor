# ─── Stage 1: Builder ────────────────────────────────────────────────────────
FROM rust:latest AS builder

# protobuf-compiler  → provides the `protoc` binary
# libprotobuf-dev    → provides the well-known .proto includes (google/protobuf/*.proto)
#                      that spark-sdk's build.rs imports at compile time
RUN apt-get update && apt-get install -y --no-install-recommends \
    protobuf-compiler \
    libprotobuf-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Copy manifest files first for better layer caching.
# The dependency layer is rebuilt only when Cargo.toml / Cargo.lock change.
COPY Cargo.toml Cargo.lock ./

# Build a dummy main so all dependencies are compiled and cached.
RUN mkdir src && echo "fn main() {}" > src/main.rs \
    && cargo build --release \
    && rm -rf src

# Now copy the real source and build the actual binary.
COPY src ./src
# Touch main.rs so Cargo knows it changed vs. the dummy stub.
RUN touch src/main.rs \
    && cargo build --release

# ─── Stage 2: Runtime ────────────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime

# Install only the runtime libraries the binary needs (OpenSSL, CA certs).
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

# Run as a non-root user for security.
RUN useradd --uid 10001 --no-create-home --shell /sbin/nologin appuser

WORKDIR /app

# Copy the compiled binary from the builder stage.
COPY --from=builder /build/target/release/cdk-payment-processor-spark /app/cdk-payment-processor-spark

# Copy the example config so users can inspect it inside the container.
COPY config.toml.example /app/config.toml.example

RUN chown -R appuser:appuser /app
USER appuser

# gRPC server port (matches the default in config.toml.example)
EXPOSE 50051

ENTRYPOINT ["/app/cdk-payment-processor-spark"]
