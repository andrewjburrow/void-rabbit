FROM rust:1-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY static ./static
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends iputils-ping ca-certificates && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /app/target/release/void-rabbit /usr/local/bin/void-rabbit
ENV ITAM_BIND=0.0.0.0:8080 ITAM_DB_PATH=/data/itam.sqlite
EXPOSE 8080
VOLUME ["/data"]
CMD ["void-rabbit"]
