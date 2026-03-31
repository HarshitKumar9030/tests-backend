FROM rust:1-bookworm AS builder

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src && echo "fn main() {}" > src/main.rs
RUN cargo build --release --locked && rm -rf src

COPY . .
RUN cargo build --release --locked

FROM debian:bookworm-slim

RUN useradd -m appuser
WORKDIR /app

COPY --from=builder /app/target/release/math_test_backend /app/server
RUN mkdir -p /app/data /app/uploads && chown -R appuser:appuser /app

USER appuser

EXPOSE 8080
ENV APP_PORT=8080
ENV APP_DB_PATH=/app/data/app.db
ENV APP_UPLOADS_DIR=/app/uploads

CMD ["/app/server"]
