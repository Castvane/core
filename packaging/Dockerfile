FROM docker.io/library/ubuntu:24.04@sha256:562111b88caa79c71391c0c4d3c2114a7d5c35915c20561fd893cb1e07929e6b
LABEL org.opencontainers.image.source="https://github.com/Castvane/core"
LABEL org.opencontainers.image.licenses="MIT"

RUN --mount=type=bind,source=restreamer-core,target=/tmp/restreamer-core,readonly \
    --mount=type=bind,source=LICENSE,target=/tmp/castvane-license,readonly \
    apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && install -d -o 10001 -g 10001 /var/lib/castvane /var/lib/castvane/media \
    && install -m 0755 /tmp/restreamer-core /usr/local/bin/restreamer-core \
    && install -D -m 0644 /tmp/castvane-license /usr/share/doc/castvane-core/LICENSE

ENV CORE_BIND_ADDR=0.0.0.0:8080 \
    CORE_DATABASE_PATH=/var/lib/castvane/core.sqlite3 \
    CORE_STORAGE_DISK_DIR=/var/lib/castvane/media \
    PATH=/opt/media-tools:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
WORKDIR /var/lib/castvane
USER 10001:10001
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/restreamer-core"]
