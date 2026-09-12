# controller と worker の 2 イメージを 1 つの Dockerfile から作る
FROM rust:1.97-slim-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release --locked --bin edamame-controller --bin edamame-worker
# distroless には shell が無く RUN でファイルを足せないため、DNS 解決に使う設定をここで作る
RUN printf 'hosts: files dns\n' > /etc/nsswitch.conf

# glibc と libgcc を含む distroless。実行ユーザーは既定の root を使う
FROM gcr.io/distroless/cc-debian12 AS runtime
COPY --from=builder /etc/nsswitch.conf /etc/nsswitch.conf

FROM runtime AS controller
COPY --from=builder /build/target/release/edamame-controller /usr/local/bin/edamame-controller
ENTRYPOINT ["edamame-controller"]

FROM runtime AS worker
COPY --from=builder /build/target/release/edamame-worker /usr/local/bin/edamame-worker
ENTRYPOINT ["edamame-worker"]
