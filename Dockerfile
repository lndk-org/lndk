# Use the official Rust image as the base image
FROM rust:1.81 AS builder

# Install protobuf compiler
RUN apt-get update && apt-get install -y protobuf-compiler git

# Create a new directory for the application
WORKDIR /app

# Clone the repository instead of copying local files
RUN git clone https://github.com/lndk-org/lndk .

# Build dependencies and the project
RUN cargo build --release

# Create a new stage with a minimal image
FROM debian:bookworm-slim AS final

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
