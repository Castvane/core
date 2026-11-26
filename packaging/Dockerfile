# syntax=docker/dockerfile:1
ARG BASE=docker.io/library/ubuntu:24.04@sha256:562111b88caa79c71391c0c4d3c2114a7d5c35915c20561fd893cb1e07929e6b
FROM ${BASE} AS assemble
COPY restreamer-core LICENSE /tmp/castvane/
RUN install -D -m 0755 /tmp/castvane/restreamer-core /out/usr/local/bin/restreamer-core \
    && install -D -m 0644 /tmp/castvane/LICENSE /out/usr/share/doc/castvane-core/LICENSE

FROM ${BASE}
ARG VERSION=development
ARG REVISION=unknown
ARG BUNDLE_FFMPEG=true
LABEL org.opencontainers.image.title="Castvane Core" \
      org.opencontainers.image.source="https://github.com/Castvane/core" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${REVISION}" \
      org.opencontainers.image.licenses="MIT"
COPY --from=assemble /out/ /
RUN apt-get update \
    && if [ "$BUNDLE_FFMPEG" = true ]; then packages="ffmpeg"; else packages=""; fi \
    && apt-get install -y --no-install-recommends ca-certificates $packages \
    && rm -rf /var/lib/apt/lists/* \
    && install -d -m 0700 -o 10001 -g 10001 /var/lib/castvane /var/lib/castvane/media \
    && if [ "$BUNDLE_FFMPEG" = true ]; then ffmpeg -version; else echo "FFmpeg supplied by operator"; fi \
        > /usr/share/doc/castvane-core/ffmpeg-build.txt \
    && dpkg-query -W '--showformat=${binary:Package}\t${Version}\t${source:Package}\t${source:Version}\n' \
        > /usr/share/doc/castvane-core/runtime-packages.tsv

ENV CORE_BIND_ADDR=0.0.0.0:8080 \
    CORE_DATABASE_PATH=/var/lib/castvane/core.sqlite3 \
    CORE_STORAGE_DISK_DIR=/var/lib/castvane/media \
    PATH=/opt/media-tools:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
WORKDIR /var/lib/castvane
USER 10001:10001
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=15s \
    CMD ["/usr/local/bin/restreamer-core", "health-check", "--url", "http://127.0.0.1:8080/healthz"]
ENTRYPOINT ["/usr/local/bin/restreamer-core"]
