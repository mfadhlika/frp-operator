FROM rust:alpine AS builder

WORKDIR /app

RUN apk add --no-cache pkgconfig openssl openssl-dev musl-dev

COPY . .

RUN cargo build --release

FROM alpine AS frp

WORKDIR /app

RUN apk add --no-cache curl

ARG FRP_VERSION=0.58.0

ARG DOCKER_PLATFORM=${TARGETPLATFORM:-linux/amd64}

RUN wget -qO- https://github.com/fatedier/frp/releases/download/v${FRP_VERSION}/frp_${FRP_VERSION}_${DOCKER_PLATFORM/\//_}.tar.gz | tar xvz --strip-components 1

FROM scratch AS runtime

WORKDIR /app

COPY --from=frp --chown=nonroot:nonroot /app/frpc .
COPY --from=builder --chown=nonroot:nonroot /app/target/release/frp-operator .

CMD ["/app/frp-operator"]