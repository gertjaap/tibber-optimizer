# Build stage
FROM rust:1.74-alpine AS builder

RUN apk add --no-cache musl-dev openssl-dev openssl-libs-static pkgconfig

WORKDIR /app
COPY Cargo.toml Cargo.lock* ./
COPY src ./src

# Build with static linking for Alpine
ENV OPENSSL_STATIC=1
ENV OPENSSL_LIB_DIR=/usr/lib
ENV OPENSSL_INCLUDE_DIR=/usr/include

RUN cargo build --release

# Runtime stage
FROM alpine:3.19

RUN apk add --no-cache ca-certificates tzdata

WORKDIR /app

COPY --from=builder /app/target/release/tibber-optimizer /app/tibber-optimizer

# Home Assistant addon expects config in /data
VOLUME /data

ENV RUST_LOG=tibber_optimizer=info

CMD ["/app/tibber-optimizer"]
