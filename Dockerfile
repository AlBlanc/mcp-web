# syntax=docker/dockerfile:1

# -----------------------------------------------------------------------------
# Stage 1: Build the Rust binary
# -----------------------------------------------------------------------------
FROM rust:bookworm AS builder

WORKDIR /usr/src/mcp-web

# Pre-cache dependencies
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release
RUN rm -rf src

# Build actual binary
COPY src src
RUN touch src/main.rs && cargo build --release

# -----------------------------------------------------------------------------
# Stage 2: Minimal runtime image with Chromium for SPA rendering
# -----------------------------------------------------------------------------
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    chromium \
    fonts-liberation \
    fonts-noto-color-emoji \
    && rm -rf /var/lib/apt/lists/*

# Environment setup
ENV CHROME_PATH=/usr/bin/chromium
ENV MCP_WEB_NO_SANDBOX=1
ENV RUST_LOG=info

# Copy compiled binary
COPY --from=builder /usr/src/mcp-web/target/release/mcp-web /usr/local/bin/mcp-web

# Expose HTTP / SSE port
EXPOSE 8000

# Default entrypoint starts the multi-client HTTP/SSE server
ENTRYPOINT ["mcp-web"]
CMD ["http", "--bind", "0.0.0.0:8000"]
