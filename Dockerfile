# Build ratatosk from source on Alpine (static musl binary) and ship it in a
# minimal image. Multi-arch friendly: `docker buildx build --platform
# linux/amd64,linux/arm64 .` Published images: ghcr.io/maxeliseyev/ratatosk
#
#   docker run -d --name ratatosk -p 1080:1080 \
#     -v "$PWD/server.txt:/data/server.txt:ro" ghcr.io/maxeliseyev/ratatosk
#   docker run -d --name ratatosk -p 1080:1080 ghcr.io/maxeliseyev/ratatosk \
#     -l 0.0.0.0 -s 'https://provider/sub/token'

FROM rust:1-alpine AS build
RUN apk add --no-cache build-base cmake perl git linux-headers
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build -p ratatosk --release --locked && strip target/release/ratatosk

FROM alpine:3.21
RUN apk add --no-cache ca-certificates && mkdir -p /data /root/.config/ratatosk
COPY --from=build /src/target/release/ratatosk /usr/local/bin/ratatosk
VOLUME ["/data"]
EXPOSE 1080
ENTRYPOINT ["ratatosk", "-l", "0.0.0.0", "-p", "1080"]
# server.txt mounted into /data, or pass -s URL / -u 'vless://...' instead
CMD ["-f", "/data/server.txt"]
