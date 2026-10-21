# tomyou

OBS の映像を Edamame と ImageFlux へ同時に送る、拠点向けのデスクトップアプリである。
OBS の配信先設定、RTMP の受信と分配、ImageFlux のチャンネル作成、状態表示を 1 つの GUI にまとめる。

## 役割

```text
OBS ── RTMP / H.264 + AAC ──> tomyou の RTMP サーバ
                               ├─ ffmpeg ──> edamame-publisher
                               └─ ffmpeg ──> WHIP ──> ImageFlux
```

- Edamame を主系、ImageFlux を予備系として扱う。
- `whip-script` の ImageFlux チャンネル作成、WHIP URL の生成、プレイリスト URL の取得、削除を引き継ぐ。
- ImageFlux のプレイリストや鍵を live2025-server へ登録する処理は行わない。
- Edamame の URL と鍵の登録は、既存の `edamame-publisher` に任せる。

## 必要なもの

- Rust 1.93 以降
- OBS Studio 30.2 以降（OBS WebSocket 5.5 以降）
- ffmpeg
  - Windows の配布ビルドでは実行ファイルに埋め込む。
  - Linux と macOS では外部の ffmpeg を使う。
- OBS の入力は H.264、B フレームなし、AAC 音声を前提とする。
- OBS のキーフレーム間隔は 2 秒を推奨する。publisher の 6 秒セグメントと両立する。

Linux でビルドする場合は、egui と資格情報ストアの開発パッケージを入れる。

```bash
sudo apt-get install \
  pkg-config libx11-dev libxkbcommon-dev libwayland-dev \
  libgl1-mesa-dev libegl1-mesa-dev libssl-dev \
  libdbus-1-dev libsecret-1-dev
```

## ビルドと起動

```bash
cargo build --release -p tomyou
cargo run -p tomyou
```

設定は `%APPDATA%\tomyou\config.toml`、`~/Library/Application Support/tomyou/config.toml`、
`~/.config/tomyou/config.toml` のいずれかへ保存される。
初回は GUI の「設定」から OBS、publisher、ImageFlux を入力する。

設定例は [`config.example.toml`](./config.example.toml) を参照する。
`ffmpeg.path` を空欄にすると、PATH、`TOMYOU_FFMPEG`、標準的なインストール先の順に探す。

## ffmpeg の機能

起動時に次の機能を実際に確認する。

- WHIP muxer
- DTLS
- libopus encoder
- RTMP protocol
- FLV demuxer
- AAC decoder

不足している場合は画面に不足機能を表示する。
ImageFlux 側だけを諦め、Edamame 側の送信は継続する。
勝手に ffmpeg をダウンロードしたり、別の実行ファイルへ置き換えたりしない。

### Windows の埋め込みビルド

`TOMYOU_FFMPEG_PATH` に WHIP 対応の `ffmpeg.exe` を指定し、`bundled-ffmpeg` を有効にする。

```bash
TOMYOU_FFMPEG_PATH=/path/to/ffmpeg.exe \
  cargo build --release -p tomyou --features bundled-ffmpeg
```

実行時にユーザー領域へ展開し、ハッシュが一致しない場合は書き直す。

## 操作

1. OBS を起動し、OBS WebSocket を有効にする。
2. `tomyou` の設定を入力して保存する。
3. 「配信を開始」を押す。
4. OBS の配信先が `rtmp://127.0.0.1:1935/live/<channel>` に設定され、OBS の配信が始まる。
5. `tomyou` が Edamame と ImageFlux の送信状態、ImageFlux の HLS URL を表示する。
6. 終了時は「配信を停止」を押す。OBS の配信先は、変更していなければ元へ戻る。

OBS が既に配信中の場合は開始しない。
OBS WebSocket だけが切れても、RTMP 入力が続く間は送信を維持する。

## 制約

- ImageFlux の HLS を live2025-server へ登録しない。そのため、この版だけでは視聴 client の自動フォールバックは完成しない。
- ImageFlux の鍵取得、鍵配布、暗号化解除は行わない。既存の鍵 URI 設定は保持する。
- 1 台につき 1 チャンネルとする。
- OBS の置き換え、録画、映像プレビュー、複数チャンネル同時配信は行わない。
- `tomyou` と `whip-script` を同じ OBS に対して同時に動かさない。
- OBS、`tomyou`、拠点 PC、拠点回線は両系で共有する。独立した冗長化ではない。
- 上り帯域は原則として映像約 2 本分必要になる。

## テスト

```bash
cargo fmt --all --check
cargo clippy -p tomyou --all-targets -- -D warnings
cargo test -p tomyou
```

実 ImageFlux と実 OBS を使う確認は [`docs/tomyou.md`](../../docs/tomyou.md) にまとめる。

## ライセンス

Apache License 2.0。
使用する egui、shiguredo_rtmp、obws、ffmpeg のライセンスは各配布物の表記に従う。
