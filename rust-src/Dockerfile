FROM rust:1.82 AS builder
WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates libssl3 && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/api-rd /usr/local/bin/
WORKDIR /home/useradmin/api/mainapi
EXPOSE 5000
CMD ["api-rd"]
