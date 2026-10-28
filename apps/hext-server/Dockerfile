# hext-server image. Build from the repository root:
#   docker build -f apps/hext-server/Dockerfile -t hext-server .
FROM rust:1.97.0-slim-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p hext-server

FROM debian:bookworm-slim
RUN useradd --system --create-home --home-dir /data hext
COPY --from=build /src/target/release/hext-server /usr/local/bin/hext-server
USER hext
VOLUME /data
EXPOSE 8080
# Plain HTTP: put a TLS proxy (Caddy) in front, so devices use wss://.
ENTRYPOINT ["hext-server", "--listen", "0.0.0.0:8080", "--db", "/data/hext-server.db"]
