#!/usr/bin/env bash
set -euo pipefail

# Historical MinIO Community build for interoperability testing.
image=docker.io/bitnamilegacy/minio@sha256:a26e42a4d6550d2fb7708c8ceefc60474b42c124b6f3273a081721ddf1d86b20
container=$(docker run --detach --rm --read-only --user 1001:1001     --publish 127.0.0.1::9000 --tmpfs /tmp --tmpfs /data:uid=1001,gid=1001     --env HOME=/tmp --env MINIO_ROOT_USER=fixture-key --env MINIO_ROOT_PASSWORD=fixture-secret     --entrypoint /opt/bitnami/minio/bin/minio "$image" server /data --address :9000)
trap 'docker stop -t 5 "$container" > /dev/null 2>&1 || true' EXIT

endpoint="http://$(docker port "$container" 9000/tcp)"
bucket_ready=false
for _ in {1..60}; do
    if curl --noproxy '*' --fail --silent --max-time 2 --request PUT         --aws-sigv4 'aws:amz:us-east-1:s3' --user fixture-key:fixture-secret         "$endpoint/phase3" > /dev/null; then
        bucket_ready=true
        break
    fi
    sleep 0.5
done
if [[ "$bucket_ready" != true ]]; then
    docker logs "$container" >&2
    exit 1
fi
PHASE3_S3_ENDPOINT="$endpoint" cargo test --locked --test media     s3_multipart_ranged_and_public_delivery -- --exact --ignored --nocapture
