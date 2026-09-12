# Edamame

ライブ HLS のセグメントファイルを controller が正規データとして保管し、worker がキャッシュして client に配信する。

## 必要な環境

- Rust 1.85 以降（edition 2024）
- controller と worker が同じ network から到達できること

## ビルド

```bash
cargo build --workspace
```

## 起動

controller を起動する。

```bash
cargo run -p edamame-controller -- \
  --bind 127.0.0.1:9000 \
  --storage-dir .edamame/controller \
  --playlist-segments 6 \
  --sync-interval-ms 1000 \
  --sync-timeout-ms 10000 \
  --cors-origin '*'
```

worker を起動する。

```bash
cargo run -p edamame-worker -- \
  --worker-id worker-a \
  --bind 127.0.0.1:9001 \
  --public-url http://127.0.0.1:9001 \
  --controller-url http://127.0.0.1:9000 \
  --cache-dir .edamame/worker-a \
  --cache-limit 12 \
  --cors-origin '*'
```

## Dockerでの起動

`compose.yaml` の例：

```yaml
services:
  controller:
    image: ghcr.io/sohosai/edamame-controller:latest
    command:
      - --bind=0.0.0.0:9000
      - --storage-dir=/data
      - --playlist-segments=6
      - --sync-interval-ms=1000
      - --sync-timeout-ms=10000
      - --cors-origin=*
    ports:
      - "9000:9000"
    volumes:
      - controller-storage:/data

  worker:
    image: ghcr.io/sohosai/edamame-worker:latest
    depends_on:
      - controller
    command:
      - --worker-id=worker-a
      - --bind=0.0.0.0:9001
      - --public-url=http://127.0.0.1:9001
      - --controller-url=http://controller:9000
      - --cache-dir=/cache
      - --cache-limit=12
      - --cors-origin=*
    ports:
      - "9001:9001"
    volumes:
      - worker-cache:/cache

volumes:
  controller-storage:
  worker-cache:
```

## 動作を確認する

セグメントファイルを PUT する。

```bash
printf 'segment-101' > /tmp/101.ts
curl -i -X PUT \
  --data-binary @/tmp/101.ts \
  'http://127.0.0.1:9000/segments/live/720p/101.ts?duration_ms=6000&bandwidth=3000000&resolution=1280x720'
```

worker が sync するまでプレイリストは 503 を返すことがある。
503 の間は再試行する。

```bash
curl -i --retry 30 --retry-delay 1 --retry-connrefused \
  http://127.0.0.1:9000/channels/live/720p.m3u8
```

プレイリストに worker の URL が書かれていることを確認し、セグメントファイルを取得する。

```bash
curl -i http://127.0.0.1:9001/segments/live/720p/101.ts
```

## 開発

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## 制約

- controller のメタデータはメモリ上だけにあり、再起動すると失われる。再起動後は publisher がセグメントファイルを再 PUT する。
- worker のキャッシュは `--cache-dir` に保存され、再起動時に走査して復元する。
- 認証と TLS はない。private network で使う。
