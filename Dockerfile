FROM rust:1-bookworm AS chef
WORKDIR /src
RUN cargo install cargo-chef --locked
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM rust:1-bookworm AS cook
WORKDIR /src
COPY --from=chef /usr/local/cargo/bin/cargo-chef /usr/local/cargo/bin/cargo-chef
COPY --from=chef /src/recipe.json recipe.json
RUN cargo chef cook --release -p arb-engine --recipe-path recipe.json

FROM rust:1-bookworm AS build
WORKDIR /src
COPY --from=cook /usr/local/cargo /usr/local/cargo
COPY --from=cook /src/target /src/target
COPY . .
RUN cargo build --locked --release -p arb-engine

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/arb-engine /usr/local/bin/arb-engine
COPY config.toml /etc/arb-engine/config.toml
WORKDIR /data
EXPOSE 8787
ENTRYPOINT ["arb-engine", "--config", "/etc/arb-engine/config.toml"]
