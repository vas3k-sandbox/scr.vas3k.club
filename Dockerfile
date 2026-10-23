FROM rust:1.99.1-trixie AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM debian:trixie-slim
WORKDIR /app
COPY --from=builder /src/target/release/scr /usr/local/bin/scr
COPY config.yml ./
COPY fonts ./fonts
ENV SCR_LISTEN=0.0.0.0:8202
USER 65532:65532
EXPOSE 8202
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s CMD ["scr", "--healthcheck"]
ENTRYPOINT ["scr"]
