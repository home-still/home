FROM docker.io/library/rust:latest AS builder
WORKDIR /build
COPY . .
RUN cargo build --release -p hs-scribe \
    --features server --bin hs-scribe-server

FROM docker.io/library/debian:trixie-slim

ARG PDFIUM_VERSION=7749
ARG TARGETARCH
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
       ca-certificates curl libgcc-s1 \
    && PDFIUM_ARCH=$(case "$TARGETARCH" in arm64) echo "arm64";; *) echo "x64";; esac) \
    && curl -fsSL \
       "https://github.com/bblanchon/pdfium-binaries/releases/download/chromium%2F${PDFIUM_VERSION}/pdfium-linux-${PDFIUM_ARCH}.tgz" \
       | tar -xz -C /usr \
    && ldconfig \
    && apt-get remove -y curl \
    && apt-get autoremove -y \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder \
    /build/target/release/hs-scribe-server \
    /usr/local/bin/

VOLUME ["/models"]

ENV HS_SCRIBE_LAYOUT_MODEL_PATH=/models/pp-doclayoutv3.onnx
ENV HS_SCRIBE_TABLE_MODEL_PATH=/models/slanet-plus.onnx

EXPOSE 7432
CMD ["hs-scribe-server", "--host", "0.0.0.0", "--port", "7432"]
