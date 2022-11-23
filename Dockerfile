FROM rust:1.65-alpine

RUN apk add --no-cache musl-dev

RUN mkdir -p /usr/src/app
WORKDIR /usr/src/app
ADD Cargo.toml Cargo.toml
ADD Cargo.lock Cargo.lock
RUN mkdir src && touch src/lib.rs
RUN cargo build --release

RUN rm -r src
ADD src src
RUN cargo build --release
RUN ln /usr/src/app/target/release/boredphoton /usr/bin/boredphoton

RUN mkdir /bot
RUN adduser --uid 1000 --disabled-password --home /bot boredphoton
WORKDIR /bot
ENTRYPOINT ["boredphoton"]
