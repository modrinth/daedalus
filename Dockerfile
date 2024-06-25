FROM rust:1.79.0 as build
ENV PKG_CONFIG_ALLOW_CROSS=1

WORKDIR /usr/src/daedalus
COPY . .
RUN cargo build --release


FROM debian:bullseye-slim

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && apt-get clean \
 && rm -rf /var/lib/apt/lists/*

RUN update-ca-certificates

COPY --from=build /usr/src/daedalus/target/release/daedalus_client /daedalus/daedalus_client
WORKDIR /daedalus_client

CMD /daedalus/daedalus_client
