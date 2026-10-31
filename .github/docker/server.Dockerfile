# Release image of the h3 server: wraps the prebuilt static musl binaries
# from the release workflow (dist/<arch>/ratatosk-server).
FROM alpine:3.21
ARG TARGETARCH
RUN apk add --no-cache ca-certificates && mkdir -p /etc/ratatosk-server
COPY dist/${TARGETARCH}/ratatosk-server /usr/local/bin/ratatosk-server
EXPOSE 443/udp
ENTRYPOINT ["ratatosk-server"]
CMD ["-c", "/etc/ratatosk-server/server.toml"]
