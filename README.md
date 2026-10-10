# Edamame

ライブ HLS のセグメントファイルを controller が正規データとして保管し、worker がキャッシュして client に配信する。

## 必要な環境

- Rust 1.85 以降（edition 2024）
- controller と worker が同じ network から到達できること
- publisher を動かす場合は、NVEnc と QSV と SRT と RTMP を含む ffmpeg。
  NVEnc を使う場合は NVIDIA の GPU とドライバ、QSV を使う場合は Intel の GPU と VA-API のドライバと oneVPL の GPU ランタイム。
  4K の超解像（`mode = "upscale"`）を使う場合は Intel VSR (raisr) を組み込んだ ffmpeg と OpenCL のランタイムが要る（publisher のイメージにはどちらも同梱される）

## ビルド

```bash
cargo build --workspace
```

## 起動

controller を起動する。

```bash
cargo run -p edamame-controller -- \
  --bind 127.0.0.1:9000 \
  --storage-capacity 512MiB \
  --playlist-segments 6 \
  --sync-interval-ms 1000 \
  --sync-timeout-ms 10000 \
  --publish-wait-ms 2000 \
  --auth-token example-token \
  --cors-origin '*'
```

worker を起動する。

```bash
cargo run -p edamame-worker -- \
  --worker-id worker-a \
  --bind 127.0.0.1:9001 \
  --public-url http://127.0.0.1:9001 \
  --controller-url http://127.0.0.1:9000 \
  --cache-limit 12 \
  --auth-token example-token \
  --cors-origin '*'
```

controller と worker の option は環境変数でも指定できる。
環境変数の名前は option 名の `-` を `_` に変えて `EDAMAME_` を付けたものにする（例: `--bind` は `EDAMAME_BIND`）。
コマンドライン引数と環境変数の両方を指定した場合はコマンドライン引数を優先する。
複数の worker を置く場合は `--weight` で相対的な配信能力を指定する。省略すると 1 になる。
`--auth-token` は controller と worker で同じ値にし、推測しにくい値に変える。
publisher もセグメントファイルの PUT に `Authorization: Bearer {token}` を付ける。
省略すると PUT の認証は無効になり、controller が起動時に警告を出す。

## publisher

publisher は `crates/edamame-publisher` にあり、SRT または RTMP で受け取った映像を NVEnc や QSV でエンコードしてセグメントを作り、
controller へ PUT する。GPU と ffmpeg が要るため、controller と worker とは別の機械で動かす。
QSV を使う場合は、Intel VSR (raisr) で 4K へ超解像する quality も作れる（`mode = "upscale"`）。

```bash
cargo run --release -p edamame-publisher -- \
  --config crates/edamame-publisher/publisher.example.toml \
  --controller-url http://127.0.0.1:9000 \
  --auth-token example-token
```

設定、処理の流れ、暗号化の仕様、動作確認、失敗時の扱いは [publisher](./docs/publisher.md) に書く。

## Dockerでの起動

`compose.yaml` の例：

```yaml
services:
  controller:
    image: ghcr.io/sohosai/edamame-controller:latest
    environment:
      EDAMAME_BIND: 0.0.0.0:9000
      EDAMAME_STORAGE_CAPACITY: "512MiB"
      EDAMAME_PLAYLIST_SEGMENTS: 6
      EDAMAME_SYNC_INTERVAL_MS: 1000
      EDAMAME_SYNC_TIMEOUT_MS: 10000
      EDAMAME_PUBLISH_WAIT_MS: 2000
      EDAMAME_AUTH_TOKEN: example-token
      EDAMAME_CORS_ORIGIN: "*"
    ports:
      - "9000:9000"

  worker:
    image: ghcr.io/sohosai/edamame-worker:latest
    depends_on:
      - controller
    environment:
      EDAMAME_WORKER_ID: worker-a
      EDAMAME_BIND: 0.0.0.0:9001
      EDAMAME_PUBLIC_URL: http://127.0.0.1:9001
      EDAMAME_CONTROLLER_URL: http://controller:9000
      EDAMAME_CACHE_LIMIT: 12
      EDAMAME_AUTH_TOKEN: example-token
      EDAMAME_CORS_ORIGIN: "*"
    ports:
      - "9001:9001"
```

publisher のイメージは raisr 入りの ffmpeg をビルドして Intel のドライバと一緒に含むため、別のターゲットで作る（ビルドに時間がかかる）。
NVIDIA の GPU は NVIDIA Container Toolkit で渡し、Intel の GPU は `--device /dev/dri:/dev/dri` で渡す。

```bash
docker build --target publisher -t edamame-publisher .
```

## 動作を確認する

セグメントファイルを PUT する。

```bash
printf 'segment-1' > /tmp/segment-1.ts
curl -i -X PUT \
  -H 'Authorization: Bearer example-token' \
  --data-binary @/tmp/segment-1.ts \
  'http://127.0.0.1:9000/segments/live/720p/segment-1?duration_ms=6000&bandwidth=3000000&resolution=1280x720'
```

controller が割り当てたセグメント番号がレスポンスの `sequence` に入る。
この例では 0 になる。

最初の公開範囲ができるまで、プレイリストは 503 を返すことがある。
公開範囲ができた後は、新しいセグメントファイルを PUT しても `--publish-wait-ms` の間は前のプレイリストを返す。
503 が続く場合は、その quality の公開範囲を保持している worker がいるか確認する。

```bash
curl -i --retry 30 --retry-delay 1 --retry-connrefused \
  http://127.0.0.1:9000/channels/live/720p.m3u8
```

プレイリストに worker の URL が書かれていることを確認し、セグメントファイルを取得する。

```bash
curl -i http://127.0.0.1:9001/segments/live/720p/0.ts
```

## 開発

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## 制約

- controller のセグメントファイル、メタデータ、セグメント番号はメモリ上だけにあり、再起動すると失われる。保存容量は `--storage-capacity` で指定し、超えた分は古いセグメントファイルから削除する。
- publisher は再起動後もセグメントファイルを PUT し続ける。controller の再起動後のセグメント番号は 0 から始まる。
- 同じセグメントには quality をまたいで同じセグメント識別子を付ける。
- セグメントファイルをプレイリストに反映するまでには、`--publish-wait-ms` と worker の sync と取得の分だけ遅れる。
- quality ごとに公開範囲は独立して決まり、メディアプレイリストは前進・後退・飛び得る。
- worker の `--cache-limit` は controller の `--playlist-segments` より 1 以上大きくすることを推奨する。小さい場合、前の公開範囲を保持できず、controller が次の sync で保持されている最新の連続範囲へ切り替える。
- worker のキャッシュはメモリ上だけにあり、再起動すると失われる。worker は登録に成功した時点でキャッシュを捨てて取り直す。
- PUT の認証は controller と worker の `--auth-token` で行い、GET と sync の POST には認証がない。TLS はないため private network で使う。
