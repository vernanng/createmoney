FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --locked --release -p arb-engine

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/arb-engine /usr/local/bin/arb-engine
COPY config.toml /etc/arb-engine/config.toml
WORKDIR /data
EXPOSE 8787
ENTRYPOINT ["arb-engine", "--config", "/etc/arb-engine/config.toml"]
