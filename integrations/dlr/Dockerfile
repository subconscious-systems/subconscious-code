FROM rust:1.94.1-bookworm AS builder
WORKDIR /src
COPY . .
RUN cargo build --locked --release -p dlr-sidecar --bin dlr-sidecar

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install --no-install-recommends -y ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --user-group --uid 10001 --home /var/lib/dlr dlr \
    && install -d -o dlr -g dlr /var/lib/dlr
COPY --from=builder /src/target/release/dlr-sidecar /usr/local/bin/dlr-sidecar
USER dlr
ENV DLR_LISTEN=0.0.0.0:32180 \
    DLR_WAL=/var/lib/dlr/receiver.wal \
    DLR_SYNC_WAL=true
VOLUME ["/var/lib/dlr"]
EXPOSE 32180
HEALTHCHECK --interval=10s --timeout=2s --retries=3 \
    CMD curl --fail --silent http://127.0.0.1:32180/readyz || exit 1
ENTRYPOINT ["/usr/local/bin/dlr-sidecar"]
