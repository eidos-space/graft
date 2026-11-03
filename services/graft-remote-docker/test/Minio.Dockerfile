# Test fixture only; build official MinIO source instead of relying on retired images.
FROM golang:1.24-bookworm@sha256:1a6d4452c65dea36aac2e2d606b01b4a029ec90cc1ae53890540ce6173ea77ac AS build
WORKDIR /src
RUN git init && git remote add origin https://github.com/minio/minio.git && \
    git fetch --depth 1 origin f2619d1f629db75c4282329352f4aaf79caba436 && \
    git checkout --detach FETCH_HEAD && \
    test "$(git rev-parse HEAD)" = f2619d1f629db75c4282329352f4aaf79caba436
RUN CGO_ENABLED=0 go build -trimpath -o /minio .
FROM node:24-bookworm-slim@sha256:c86ecead9e55a5210ac1727de38a4ca840bf03dcaf34dc9749addbf383b1920c
LABEL org.opencontainers.image.source="https://github.com/minio/minio" \
      org.opencontainers.image.revision="f2619d1f629db75c4282329352f4aaf79caba436" \
      org.opencontainers.image.licenses="AGPL-3.0-only" \
      org.opencontainers.image.description="Isolated Graft S3 test fixture; not a deployment artifact"
COPY --from=build /minio /usr/local/bin/minio
COPY --from=build /src/LICENSE /usr/share/licenses/minio/LICENSE
ENTRYPOINT ["/usr/local/bin/minio"]
