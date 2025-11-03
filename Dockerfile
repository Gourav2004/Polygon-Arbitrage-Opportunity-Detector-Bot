# Step 1: Build the Rust project
FROM rust:1.73 as builder

# Create app directory
WORKDIR /app

# Copy all project files into the container
COPY . .

# Build the project in release mode
RUN cargo build --release

# Step 2: Create a small runtime image
FROM debian:bullseye-slim

# Install necessary system dependencies
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*

# Set work directory
WORKDIR /app

# Copy the compiled binary from builder
COPY --from=builder /app/target/release /app

# Copy static files if any
COPY --from=builder /app/static /app/static

# Expose port 8080 (you can change it if your app uses another port)
EXPOSE 8080

# Run your Rust executable (replace "polygon_arbitrage_opportunity_detector_boat" if your binary name is different)
CMD ["./polygon_arb_bot"]

