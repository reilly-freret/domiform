# ---- Build stage ----
FROM rust:alpine AS builder

# musl-dev provides the C toolchain/headers needed to statically link against musl.
RUN apk add --no-cache musl-dev

WORKDIR /build
COPY . .

# rust:alpine already targets musl by default, so this produces a static binary
# that runs on a bare Alpine image.
RUN cargo build --release --bin domiform

# ---- Runtime stage ----
FROM alpine:latest

COPY --from=builder /build/target/release/domiform /usr/local/bin/domiform

ENTRYPOINT ["domiform"]
