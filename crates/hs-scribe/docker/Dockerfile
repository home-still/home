FROM rust:1.82-bookworm AS builder                                          
WORKDIR /build                                                              
COPY . .                                                                    
RUN cargo build --release -p hs-scribe \                                    
    --features server --bin hs-scribe-server

FROM debian:bookworm-slim

ARG PDFIUM_VERSION=6611
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
    ca-certificates curl libgcc-s1 \
    && curl -fsSL \
    "https://github.com/nicholasgasior/pdfium-binaries/releases/download/chromium/${PDFIUM_VERSION}/pdfium-linux-x64.tgz" \
    | tar -xz -C /usr/lib \
    && ldconfig \
    && apt-get remove -y curl \
    && apt-get autoremove -y \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder \
    /build/target/release/hs-scribe-server \
    /usr/local/bin

VOLUME ["/models"]

ENV HS_SCRIBE_LAYOUT_MODEL_PATH=/models/pp-doclayoutv3.onnx     
ENV HS_SCRIBE_TABLE_MODEL_PATH=/models/slanet-plus.onnx                     
ENV HS_SCRIBE_OLLAMA_URL=http://ollama:11434                    

EXPOSE 7432                                                     
CMD ["hs-scribe-server", "--host", "0.0.0.0", "--port", "7432"]  