FROM rust:latest AS builder

WORKDIR /app

COPY . .

RUN cargo build --release

FROM debian:stable-slim AS runtime

WORKDIR /app

RUN apt update && apt upgrade -y

COPY --from=builder --chown=nonroot:nonroot /app/target/release/frp-operator .

CMD ["/app/frp-operator"]