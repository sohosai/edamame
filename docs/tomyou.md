# tomyou

`tomyou` は、拠点の Windows、Linux、macOS で動く配信アプリである。
OBS の映像を Edamame と ImageFlux へ同時に送り、送信先ごとの状態を GUI に表示する。

この文書では、構成、OBS と GStreamer の条件、ImageFlux の扱い、障害時の動作、検証方法を説明する。
ビルドと起動の最短手順は [`crates/tomyou/README.md`](../crates/tomyou/README.md) を参照する。

## 構成

```text
OBS ── RTMP ──> tomyou の RTMP サーバ
                  ├─ GStreamer ──> rtmpsink ──> edamame-publisher
                  │                                  ├─> controller / worker
                  │                                  └─> live2025-server に既存方式で登録
                  └─ GStreamer ──> whipclientsink ──> ImageFlux
```

`tomyou` は OBS の配信先をローカルの RTMP サーバへ変更する。
RTMP サーバは 1 接続の publish だけを受け付け、複数のローカル play 接続へフレームを分配する。

Edamame と ImageFlux への送信は、それぞれ独立した GStreamer プロセスが行う。

- Edamame 向けは H.264 と AAC を FLV に戻し、publisher へ RTMP で送る。
- ImageFlux 向けは H.264 をコピーし、音声を Opus に変換して WHIP で送る。
- ImageFlux のチャンネル作成、WHIP URL の生成、プレイリスト URL の取得、削除は `whip-script` の API 操作を引き継ぐ。
- ImageFlux のプレイリストと鍵を live2025-server へ登録する処理は行わない。
- Edamame の URL と鍵の登録は、既存の `edamame-publisher` が行う。

## OBS の条件

OBS は 30.2 以降の WebSocket 5.5 を使う。
`tomyou` は OBS WebSocket で次の操作を行う。

1. 既に配信中でないことを確認する。
2. 変更前の配信先を保存する。
3. 配信先を `rtmp_custom` の `rtmp://127.0.0.1:1935/live/<channel>` にする。
4. 配信を開始する。
5. 停止時に、配信先がまだ `tomyou` の設定なら元へ戻す。

OBS の入力は H.264、B フレームなし、AAC 音声を前提とする。
キーフレーム間隔は publisher のセグメント長を割り切る値にする。
6 秒セグメントでは 2 秒を推奨する。
OBS のシーン、ソース、解像度を `tomyou` が自動変更することはない。

OBS が既に配信中の場合は、`tomyou` は開始しない。
OBS WebSocket だけが切れても、RTMP 入力が続いている間は送信を維持する。

## GStreamer の条件

GStreamer は外部ランタイムとして用意する。アプリには同梱しない。
カスタムビルドの `rswebrtc` を使う場合は `GST_PLUGIN_PATH` を設定する。

必要な要素:

- Edamame: `rtmpsrc`、`flvdemux`、`h264parse`、`aacparse`、`flvmux`、`rtmpsink`、`queue`
- ImageFlux: `whipclientsink`、`webrtcbin`、`nicesrc`、`nicesink`、`opusenc`、`audioconvert`、`audioresample`、`queue`
- AAC デコーダ: `avdec_aac` または `faad`
- `whipclientsink` を含む `rswebrtc` プラグイン

macOS の Homebrew 環境では、ICE（WebRTC の通信経路を確立する仕組み）に必要な `nicesrc` と `nicesink` を `brew install libnice-gstreamer` で導入する。
`libnice` 単体のインストールでは GStreamer の要素は追加されない。

起動時に `gst-inspect-1.0` で要素を確認する。
不足している場合は不足名を GUI に表示する。
ImageFlux の要素だけが足りない場合は Edamame の送信を継続する。
Edamame の要素が足りない場合は配信を開始しない。

GStreamer の実行ファイルは、設定の `gstreamer.path`、`TOMYOU_GST_LAUNCH`、PATH、OS 標準の場所の順に探す。

## whipclientsink

ImageFlux には `whipclientsink` を使う。`whipsink` は使わない。

- WHIP の接続先は `signaller::whip-endpoint` に指定する。
- Link ヘッダーで ICE サーバーを設定するため、`signaller::use-link-headers=true` を指定する。
- 映像パッドは `video_%u`、音声パッドは `audio_%u` である。
- 音声を先に接続し、SDP の m-line を `audio0`、`video1` の順にする。Sora の answer は音声を先頭に返すため、この順序が必要である。
- 映像は H.264 をコピーし、音声は AAC をデコードして Opus の 48 kHz ステレオへ変換する。

パイプラインの形:

```text
rtmpsrc location="{input} live=1 flashver=tomyou" ! flvdemux name=demux
demux.audio ! aacparse ! {aac_decoder} ! audioconvert ! audioresample !
  audio/x-raw,rate=48000,channels=2 ! opusenc bitrate=128000 !
  queue max-size-time=5000000000 max-size-buffers=0 max-size-bytes=0 ! whip.audio_0
demux.video ! h264parse config-interval=-1 ! queue ! whip.video_0
whipclientsink name=whip
  signaller::whip-endpoint="{endpoint}"
  signaller::use-link-headers=true
```

ローカル RTMP への接続では、ライブ入力の指定と `flashVer` を明示する。
中継側は `flashVer` を省略した接続も受け付けるため、GStreamer の再接続で接続情報が省略されても中継を継続できる。
`flashVer` に文字列以外を送った場合や、接続先を示す `app` / `tcUrl` を省略した場合は接続を拒否する。
RTMP の確認応答（ACK）で使う累計バイト数には、接続時のハンドシェイクも含める。
接続が切断中になったら、その接続へのフレーム送信を止め、送信待ちのフレームを破棄して接続を閉じる。
途中接続では音声が映像のキーフレームより先に届くため、両方の出力で音声キューを最大 5 秒分にする。
音声キューの既定の 1 秒制限に達すると、共通の RTMP 入力が止まり、キーフレームも届かなくなる。

`gst-launch-1.0` には `-e` を付け、停止時に EOS を送る。

## ImageFlux

ImageFlux には `CreateMultistreamChannelWithHLS` でチャンネルを作る。
返された Sora URL とチャンネル ID から `https://<host>/whip/<channel_id>` を作る。
HLS 設定は `whip-script` の設定を取り込み、解像度、FPS、ビットレート、セグメント長、アーカイブ先を保持する。

プレイリスト URL は `ListPlaylistURLs` で取得し、画面に表示する。
30 秒以内に生成されない場合は「生成待ち」として表示する。
停止時はチャンネル削除を試み、失敗した場合は次回起動時に残存チャンネルとして確認できるようにする。

この版では、ImageFlux のプレイリスト URL と鍵を live2025-server へ登録しない。
また、ImageFlux の鍵取得、鍵の保存、鍵配布も行わない。
既存の `encrypt_key_uri` は保持するが、Edamame の鍵だけを返す現在のエンドポイントを ImageFlux の鍵配布先として流用できるとは扱わない。

したがって、この版だけで視聴 client の自動フォールバックを完成させない。
画面に HLS URL を表示できても、復号再生を確認できるまでは「視聴可能」と表示しない。

## 障害時の動作

| 障害 | 動作 |
| --- | --- |
| OBS に接続できない | 配信を開始しない |
| OBS が既に配信中 | 既存の配信を停止せず、開始を拒否する |
| GStreamer の Edamame 要素が不足 | 配信を開始しない |
| ImageFlux の GStreamer 要素が不足 | Edamame だけで配信を開始する |
| ImageFlux のチャンネル作成に失敗 | Edamame だけで配信を開始する |
| ImageFlux の WHIP が切れる | ImageFlux の GStreamer だけをバックオフして再起動する。Edamame は継続する |
| ImageFlux の HLS が生成されない | Edamame は継続し、画面に生成待ちを表示する |
| edamame-publisher が停止、または RTMP URL へ到達できない | ImageFlux は継続する。Edamame 側の GStreamer だけを再起動する。GUI のログに接続失敗を表示する |
| RTMP 入力が切れる | 出力を止め、OBS の再接続を待つ |
| GStreamer が起動できない | 原因を画面に表示し、バックオフして再試行する |

ImageFlux と Edamame の出力は別プロセスなので、片方の停止が他方へ直接波及しない。
ただし、OBS、`tomyou`、拠点 PC、拠点回線は両系で共有する。
これらが停止すれば両系が止まる。
上り帯域は原則として映像約 2 本分必要になる。

## 設定と秘密情報

設定は OS 標準のユーザーディレクトリへ保存する。

- Windows: `%APPDATA%\tomyou\config.toml`
- macOS: `~/Library/Application Support/tomyou/config.toml`
- Linux: `~/.config/tomyou/config.toml`

起動時に `--config <FILE>` を指定すると、そのファイルを読み込み、保存先にも使う。
GUI の「設定」にある「設定ファイル」でも、任意のパスを指定して読み込み・保存できる。
指定したファイルを読めない場合も GUI は初期値で起動し、設定画面でパスを直せる。

OBS のパスワードと ImageFlux の API トークンも設定ファイルへ保存する。
Unix では保存時に 0600 へ変更し、所有者だけが読めるようにする。
設定画面の「OS のアプリで開く」から、OS の既定アプリで TOML を直接編集できる。
ファイルが無い場合は、現在の設定を保存してから開く。
編集後は「再読込」を押す。

秘密情報を含むため、設定ファイルを配布したりリポジトリへ commit したりしない。
ログと画面のコピー機能には秘密情報を出さない。

## publisher の RTMP URL

publisher の RTMP URL は、`tomyou` を動かすホストから到達できる必要がある。

到達性はホスト名、ポート、ファイアウォール、NAT、VPN/ACL に依存する。
`nc -vz <host> <port>` などで確認する。
RTMP のチャンネル名は大文字小文字を区別する。
設定した channel と publisher の入力名を一致させる。

到達できない場合は、ローカルの RTMP 受信先で GStreamer のパイプラインを検証し、環境側の問題として切り分ける。
アプリは到達性を推測して設定を変更しない。

## 配布

GStreamer は外部ランタイムとし、アプリには同梱しない。

- Windows: 利用者が GStreamer の MSVC ランタイムと `whipclientsink` を用意する。`tomyou.exe` は PATH または設定のパスから `gst-launch-1.0.exe` を探す。
- macOS: GStreamer framework または Homebrew の GStreamer を使う。`whipclientsink` を含むことを確認する。
- Linux: 既存の GStreamer 環境を使う。導入手順はこの文書に含めない。

単一 exe 配布は行わない。
`tomyou.exe` と外部 GStreamer の組み合わせを前提にする。

## 検証

単体テストは設定、URL、ImageFlux の JSON、GStreamer のパイプラインを確認する。
実サービスを使う確認は次の順で行う。

1. `gst-inspect-1.0 whipclientsink` が成功することを確認する。
2. 実 ImageFlux のテストチャンネルを作る。
3. `videotestsrc` / `audiotestsrc` から `whipclientsink` で WHIP 送信する。
4. 配信中に ImageFlux のマスター、メディアプレイリスト、セグメントを取得する。
5. `ffprobe` で映像と音声を確認し、実 client で再生時刻の進行を確認する。
6. RTMP 入力から `whipclientsink` へ流しても同じ結果になることを確認する。
7. Edamame 向けは `rtmpsink` で publisher へ送れることを確認する。
8. 片方の GStreamer を止めても、もう片方が継続することを確認する。
9. テストで作成した ImageFlux チャンネルを削除する。

ローカル RTMP の遅延は、初回のキーフレーム待ち、再接続、HLS の遅延と分けて測る。

## 制約

- 録画、映像プレビュー、遠隔操作、複数チャンネル同時配信は行わない。
- 映像の自動再エンコード、自前 WebRTC、ImageFlux の RTMP ingest は行わない。
- `tomyou` と `whip-script` を同じ OBS に対して同時に動かさない。
- Edamame と ImageFlux の独立した冗長化ではない。
