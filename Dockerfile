# Build context should be repo root
# Dockerfile is at /Dockerfile

# --- Builder Stage ---
FROM rust:bookworm AS builder
WORKDIR /app

# Copy dependency manifests
COPY src/Cargo.toml src/Cargo.lock ./

# Create a dummy src directory to build and cache dependencies
RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release

# Now copy the actual source code and do the final build
COPY src/src ./src
# Update the timestamp of main.rs to force Cargo to recompile the real code
RUN touch src/main.rs
RUN cargo build --release

# --- Final Stage ---
FROM debian:bookworm-slim
# Deliberately no Docker CLI and no docker.sock mount (see docker-compose.yml).
# Server control goes through the `mc-control` sidecar and console commands go
# over RCON, so an RCE in this container cannot reach the Docker daemon — which
# would otherwise be root on the host via /containers/create bind mounts.
RUN apt-get update && apt-get install -y ca-certificates libssl3 && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/api-rd /usr/local/bin/
COPY src/private /home/useradmin/api/mainapi/src/private
RUN mkdir -p /home/useradmin/api/mainapi/src/content
WORKDIR /home/useradmin/api/mainapi
EXPOSE 5000
CMD ["api-rd"]
