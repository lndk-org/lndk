# Use the official Rust image as the base image
FROM rust:1.81 AS builder

# Install protobuf compiler
RUN apt-get update && apt-get install -y protobuf-compiler

# Create a new directory for the application
WORKDIR /app

# Copy the entire project
COPY . .

# Build dependencies and the project
RUN cargo build --release

# Create a new stage with a minimal image
FROM ubuntu:22.04 AS final

# Install necessary runtime dependencies
RUN apt-get update && \
    apt-get install -y libssl3 ca-certificates && \
    apt-get clean && \
    rm -rf /var/lib/apt/lists/*

# Copy the built executables from the builder stage
COPY --from=builder /app/target/release/lndk /usr/local/bin/lndk
COPY --from=builder /app/target/release/lndk-cli /usr/local/bin/lndk-cli

# Set the startup command
CMD ["lndk"]
