# Release image: wraps the prebuilt static musl binaries from the release
# workflow (dist/<arch>/ratatosk), one image for linux/amd64 and linux/arm64.
FROM alpine:3.21
ARG TARGETARCH
RUN apk add --no-cache ca-certificates && mkdir -p /data /root/.config/ratatosk
COPY dist/${TARGETARCH}/ratatosk /usr/local/bin/ratatosk
VOLUME ["/data"]
EXPOSE 1080
ENTRYPOINT ["ratatosk", "-l", "0.0.0.0", "-p", "1080"]
CMD ["-f", "/data/server.txt"]
