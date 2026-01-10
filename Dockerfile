FROM rust:1-slim-bookworm as builder

# Install build dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    libzstd-dev \
    clang \
    git \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Create a dummy project to cache dependencies
RUN cargo new --bin allman
WORKDIR /app/allman

# Copy manifest configuration
COPY Cargo.toml ./

# Build dependencies only (this layer is cached)
RUN cargo build --release
RUN rm src/*.rs

# Copy source code
COPY ./migrations ./migrations
COPY ./src ./src

# Build the actual application
# We touch main.rs to ensure a rebuild
RUN touch src/main.rs && cargo build --release

# Runtime stage
FROM debian:bookworm-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    libssl3 \
    libzstd1 \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy binary from builder
COPY --from=builder /app/allman/target/release/allman /app/allman

# Environment config
ENV RUST_LOG=info
EXPOSE 8000

CMD ["./allman"]
