# tomyou

OBS の映像を Edamame と ImageFlux へ同時に送る、拠点向けのデスクトップアプリである。
OBS の配信先設定、RTMP の受信と分配、ImageFlux のチャンネル作成、状態表示を 1 つの GUI にまとめる。

## 役割

```text
OBS ── RTMP ──> tomyou の RTMP サーバ
                  ├─ GStreamer ──> rtmpsink ──> edamame-publisher
                  └─ GStreamer ──> whipclientsink ──> ImageFlux

tomyou ── HTTPS ──> live2025-server（ImageFlux を fallback として登録）
```

- Edamame を主系、ImageFlux を予備系として扱う。
- `whip-script` の ImageFlux チャンネル作成、WHIP URL の生成、プレイリスト URL の取得、削除を引き継ぐ。
- Edamame と ImageFlux への送信は、それぞれ独立した GStreamer プロセスが行う。
- ImageFlux のプレイリスト URL と鍵を live2025-server へ fallback として登録する。
- Edamame の通常用 URL と鍵の登録は、既存の `edamame-publisher` に任せる。
- ImageFlux の鍵は ImageFlux の `GetEncryptKey` で取得し、live2025-server へ送る。ローカルには保存しない。
- RTMP で受信した FLV の AVC シーケンスヘッダー (SPS) と AAC の AudioSpecificConfig を解析し、実際の解像度・音声形式・B フレームの可能性・キーフレーム間隔を GUI に表示する。

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
- ImageFlux Live Streaming の API トークン（ImageFlux を予備系として使う場合）
- live2025-server の `STREAM_INGEST_TOKEN`（fallback 登録を使う場合）

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
OBS のパスワード、ImageFlux の API トークン、`STREAM_INGEST_TOKEN` も設定ファイルへ保存する。
Unix では設定ファイルを所有者だけが読める 0600 で保存する。
設定画面の「OS のアプリで開く」から、OS の既定アプリで TOML を直接編集できる。
編集後は「再読込」を押す。

fallback 登録を有効にする場合は、`[stream_credentials]` に live2025-server の `base_url` と `ingest_token` を指定する。
鍵 URI は `<base_url>/key/<channel_id>?fallback=1` を自動で使う。
`[stream_credentials]` が空の場合は登録せず、ImageFlux の HLS は暗号化しない。

設定例は [`config.example.toml`](./config.example.toml) を参照する。
`gstreamer.path` を空欄にすると、PATH、`TOMYOU_GST_LAUNCH`、標準的なインストール先の順に探す。

## fallback 登録

`[stream_credentials]` を設定すると、ImageFlux を live2025-server の fallback 配信として登録する。

1. `base_url` と `channel_id` から鍵 URI を作り、ImageFlux のチャンネル作成に渡す。
2. `ListPlaylistURLs` で HLS プレイリスト URL を取得する。
3. プレイリストの `#EXT-X-KEY` から `kid` を取り出し、`ImageFlux_20200707.GetEncryptKey` で 16 バイトの鍵を取得する。
4. `POST <base_url>/internal/stream-credentials?fallback=1` に、`channel_id`、ImageFlux のプレイリスト URL、鍵を登録する。

登録に成功すると、GUI に「fallback 登録: 登録済み」と表示する。
`kid` の取得や登録に失敗しても、Edamame と ImageFlux の送信は継続する。
登録 API が 400 または 401 を返した場合は再試行せず、設定エラーとして表示する。
5xx と通信エラーは 1 秒から 30 秒の間隔で再試行する。
登録の反映は live2025-server のキャッシュにより最大 10 秒、配信終了の反映は最大 5 秒遅れる。

fallback 行の有効期間は 7 日で、削除する API はない。
配信を停止すると ImageFlux のチャンネルは削除され、プレイリストを取得できなくなった行は選択対象から外れる。
ImageFlux の再接続でプレイリスト URL が変わった場合、次回の配信開始まで再登録しない。

## 入力チェック

RTMP で受信した FLV のシーケンスヘッダーを解析し、次の値を GUI に表示する。

- 映像: コーデック、解像度、profile/level
- 音声: コーデック、サンプリングレート、チャンネル数
- B フレーム: CTS offset または破棄可能フレームの有無
- キーフレーム間隔: 直前のキーフレームからの間隔

期待値は H.264、1920x1080、AAC、B フレームなし、キーフレーム間隔 2 秒である。
映像・音声・B フレームが期待値と違う場合、またはキーフレーム間隔が 2.5 秒を超える場合は該当行を赤で表示する。
入力チェックは表示だけで、配信は停止しない。

解像度は H.264 の SPS (Sequence Parameter Set) から取得する。
1920x1080 は SPS 上では 1920x1088 になるため、frame cropping を適用して 1080 に戻す。
音声のサンプリングレートとチャンネル数は AAC の AudioSpecificConfig から取得する。
FLV の音声ヘッダーのサンプリングレートは AAC では固定値なので使わない。

B フレームは FLV の CTS offset とフレーム種別による目安であり、デコード結果を完全に保証するものではない。

## リリース

`tomyou-v<version>` タグを push すると、GitHub Actions が次の OS 向けバイナリをビルドし、GitHub Release に添付する。

- Linux: `x86_64-unknown-linux-gnu`
- Windows: `x86_64-pc-windows-msvc`
- macOS: Universal (Apple Silicon / Intel)

ファイル名は `tomyou-<version>-linux-x86_64.tar.gz`、`tomyou-<version>-windows-x86_64.zip`、`tomyou-<version>-macos-universal.tar.gz` になる。
`SHA256SUMS.txt` も添付する。
GStreamer は同梱しない。各 OS に GStreamer と `whipclientsink` を用意する。

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
5. Edamame と ImageFlux の送信状態、ImageFlux の HLS URL、fallback 登録の状態、入力ストリームの映像・音声情報を表示する。
6. 終了時は「配信を停止」を押す。OBS の配信先は、変更していなければ元へ戻る。

OBS が既に配信中の場合は開始しない。
OBS WebSocket だけが切れても、RTMP 入力が続く間は送信を維持する。

## 制約

- fallback 登録は配信開始後に一度だけ行う。ImageFlux の再接続でプレイリスト URL が変わった場合は、次回の配信開始まで再登録しない。
- fallback 行を削除する API はない。停止後も行は残るが、プレイリストを取得できなくなると選択対象から外れ、7 日で鮮度切れになる。
- 7 日を超える連続配信では fallback 行が鮮度切れになる。配信を開始し直すと再登録する。
- fallback 登録に失敗しても Edamame と ImageFlux の送信は継続する。
- fallback 登録が無効な場合、ImageFlux の HLS は暗号化しない。
- 入力チェックは表示だけで、期待値と違っても配信を止めない。
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
