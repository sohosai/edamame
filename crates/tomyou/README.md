# tomyou

OBS の映像を Edamame と ImageFlux へ同時に送る、拠点向けのデスクトップアプリである。
OBS の配信先設定、RTMP の受信と分配、ImageFlux のチャンネル作成、状態表示を 1 つの GUI にまとめる。

## 役割

```text
OBS ── RTMP ──> tomyou の RTMP サーバ
                  ├─ GStreamer ──> rtmpsink ──> edamame-publisher
                  └─ GStreamer ──> whipclientsink ──> ImageFlux
```

- Edamame を主系、ImageFlux を予備系として扱う。
- `whip-script` の ImageFlux チャンネル作成、WHIP URL の生成、プレイリスト URL の取得、削除を引き継ぐ。
- Edamame と ImageFlux への送信は、それぞれ独立した GStreamer プロセスが行う。
- ImageFlux のプレイリストや鍵を live2025-server へ登録する処理は行わない。
- Edamame の URL と鍵の登録は、既存の `edamame-publisher` に任せる。

## 必要なもの

- Rust 1.93 以降
- OBS Studio 30.2 以降（OBS WebSocket 5.5 以降）
- GStreamer
  - `gst-launch-1.0` と `gst-inspect-1.0`
  - `rswebrtc` の `whipclientsink`
  - `rtmpsrc`、`flvdemux`、`h264parse`、`aacparse`、`flvmux`、`rtmpsink`
  - `avdec_aac` または `faad`、`opusenc`、`audioconvert`、`audioresample`、`queue`
- OBS の入力は H.264、B フレームなし、AAC 音声を前提とする。
- OBS のキーフレーム間隔は 2 秒を推奨する。publisher の 6 秒セグメントと両立する。

GStreamer はアプリに同梱しない。OS ごとに外部のランタイムを用意する。
カスタムビルドの `rswebrtc` を使う場合は `GST_PLUGIN_PATH` を設定する。

Linux でビルドする場合は、egui の開発パッケージを入れる。

```bash
sudo apt-get install \
  pkg-config libx11-dev libxkbcommon-dev libwayland-dev \
  libgl1-mesa-dev libegl1-mesa-dev libssl-dev \
  libxkbcommon-x11-0
```

## ビルドと起動

```bash
cargo build --release -p tomyou

# OS 標準の設定ファイルを使う
cargo run -p tomyou

# 特定の設定ファイルを読み込む
cargo run -p tomyou -- --config /path/to/config.toml
```

設定は `%APPDATA%\tomyou\config.toml`、`~/Library/Application Support/tomyou/config.toml`、
`~/.config/tomyou/config.toml` のいずれかへ保存される。
`--config` を指定した場合は、そのパスを読み込み、保存先にも使う。
GUI の「設定」でも設定ファイルのパスを指定して読み込み・保存できる。
OBS のパスワードと ImageFlux の API トークンも設定ファイルへ保存する。
Unix では設定ファイルを所有者だけが読める 0600 で保存する。
設定画面の「OS のアプリで開く」から、OS の既定アプリで TOML を直接編集できる。
編集後は「再読込」を押す。

設定例は [`config.example.toml`](./config.example.toml) を参照する。
`gstreamer.path` を空欄にすると、PATH、`TOMYOU_GST_LAUNCH`、標準的なインストール先の順に探す。

## GStreamer の機能

起動時に次の要素を確認する。

- Edamame: `rtmpsrc`、`flvdemux`、`h264parse`、`aacparse`、`flvmux`、`rtmpsink`、`queue`
- ImageFlux: `whipclientsink`、`opusenc`、`audioconvert`、`audioresample` など
- AAC デコーダ: `avdec_aac` または `faad`

`whipclientsink` では、音声パッドを先に接続して SDP の m-line を `audio0`、`video1` の順にする。
Sora の answer は音声を先頭に返すため、この順序が必要である。
`whip-endpoint` ではなく `signaller::whip-endpoint` を使う。

不足要素がある場合は、不足名を GUI のログに表示する。
ImageFlux 側だけが不足している場合は Edamame の送信を継続する。
GStreamer のインストールをアプリが自動実行することはない。

## 操作

1. OBS を起動し、OBS WebSocket を有効にする。
2. `tomyou` の設定を入力して保存する。
3. 「配信を開始」を押す。
4. OBS の配信先が `rtmp://127.0.0.1:1935/live/<channel>` に設定され、OBS の配信が始まる。
5. Edamame と ImageFlux の送信状態、ImageFlux の HLS URL を表示する。
6. 終了時は「配信を停止」を押す。OBS の配信先は、変更していなければ元へ戻る。

OBS が既に配信中の場合は開始しない。
OBS WebSocket だけが切れても、RTMP 入力が続く間は送信を維持する。

## 制約

- ImageFlux の HLS を live2025-server へ登録しない。そのため、この版だけでは視聴 client の自動フォールバックは完成しない。
- ImageFlux の鍵取得、鍵配布、暗号化解除は行わない。既存の鍵 URI 設定は保持する。
- 1 台につき 1 チャンネルとする。
- OBS の置き換え、録画、映像プレビュー、複数チャンネル同時配信は行わない。
- Windows の単一 exe 配布は行わない。GStreamer は外部ランタイムとする。
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
使用する egui、shiguredo_rtmp、obws、GStreamer、rswebrtc のライセンスは各配布物の表記に従う。
