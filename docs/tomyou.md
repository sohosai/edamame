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
                  │                                  └─> live2025-server に通常用として登録
                  └─ GStreamer ──> whipclientsink ──> ImageFlux

tomyou ── HTTPS ──> live2025-server に ImageFlux を fallback として登録
```

`tomyou` は OBS の配信先をローカルの RTMP サーバへ変更する。
RTMP サーバは 1 接続の publish だけを受け付け、複数のローカル play 接続へフレームを分配する。

Edamame と ImageFlux への送信は、それぞれ独立した GStreamer プロセスが行う。

- Edamame 向けは H.264 と AAC を FLV に戻し、publisher へ RTMP で送る。
- ImageFlux 向けは H.264 をコピーし、音声を Opus に変換して WHIP で送る。
- ImageFlux のチャンネル作成、WHIP URL の生成、プレイリスト URL の取得、削除は `whip-script` の API 操作を引き継ぐ。
- ImageFlux のプレイリスト URL と鍵を live2025-server へ fallback として登録する。
- Edamame の通常用 URL と鍵の登録は、既存の `edamame-publisher` が行う。

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

実際に届いた映像・音声の値は、GUI の入力チェックで確認できる。

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
Windows では GUI から起動する `gst-inspect-1.0` と `gst-launch-1.0` を `CREATE_NO_WINDOW` 付きで起動し、コンソールウィンドウを開かない。
機能検査だけで十数回起動するため、抑止しないと起動のたびにウィンドウが並ぶ。

## 入力チェック

`tomyou` は RTMP で受信した FLV の映像・音声シーケンスヘッダーを解析し、実際の入力値を GUI に表示する。
映像・音声の期待値と違う場合、B フレームの可能性がある場合、キーフレーム間隔が 2.5 秒を超える場合は該当行を赤で表示する。
配信は停止しない。

- 映像: H.264、1920x1080、profile/level。
  AVC の SPS (Sequence Parameter Set) を解析する。
  SPS は H.264 の符号化パラメータで、解像度、profile、level などを持つ。
  1920x1080 は SPS 上では 1920x1088 になるため、frame cropping を適用して 1080 に戻す。
- 音声: AAC、サンプリングレート、チャンネル数。
  AAC の AudioSpecificConfig を解析する。
  FLV の音声ヘッダーのサンプリングレートは AAC では固定値なので使わない。
- B フレーム: FLV の CTS offset が 0 以外、または破棄可能フレームを観測した場合に「CTS offset あり」と表示する。
  これは目安であり、デコード結果を完全に保証するものではない。
- キーフレーム間隔: 直前のキーフレームからの時間。
  2.5 秒を超える場合は赤で表示する。
- 入力チェック: 映像・音声・B フレーム・キーフレーム間隔をまとめて OK / 要確認で表示する。

受信したシーケンスヘッダーがまだない項目は「待機中」「計測中」と表示する。
値は OBS の設定値ではなく、実際に届いたストリームの値である。

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
確認応答の間隔は接続開始時から既定値を使う。
`connect` コマンドを受け取る前に ACK を返すことがあり、間隔が 0 のままだとその小さな送信を「ACK 未着」と誤判定して切断してしまう。
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

fallback 登録を有効にすると、ImageFlux の鍵 URI は `<stream_credentials.base_url>/key/<channel_id>?fallback=1` になる。
ImageFlux はこの URI に `&kid=...` を追加してプレイリストへ出力する。
`tomyou` はプレイリストの `#EXT-X-KEY` から `kid` を取り出し、`ImageFlux_20200707.GetEncryptKey` で 16 バイトの鍵を取得する。

取得した鍵とプレイリスト URL は、`POST <stream_credentials.base_url>/internal/stream-credentials?fallback=1` に登録する。
`channel_id` は ImageFlux のチャンネル ID ではなく、`uni`、`1A`、`kaikan` のいずれかである。
登録は配信開始後、プレイリストが生成されてから行う。
鍵はメモリ上だけで扱い、設定ファイルやログには保存しない。

登録に失敗しても、Edamame と ImageFlux の送信は継続する。
通信エラーと 5xx は 1 秒から 30 秒の間隔で再試行し、400 と 401 は設定エラーとして打ち切る。
fallback 行の有効期間は 7 日で、削除する API はない。
停止後はプレイリストを取得できなくなった行が選択対象から外れ、7 日の鮮度切れでも外れる。
ImageFlux の再接続でプレイリスト URL が変わった場合は、次回の配信開始まで再登録しない。
登録の反映はサーバのキャッシュにより最大 10 秒、配信終了の反映は最大 5 秒遅れる。

通常用の鍵が未登録の場合、通常用のプレイリストが生きていても鍵取得は 404 になり、fallback の鍵へは自動で切り替わらない。
通常用の鍵の登録は `edamame-publisher` が行う。

画面に HLS URL を表示できても、視聴 client での復号再生を確認できるまでは「視聴可能」と表示しない。

## 障害時の動作

| 障害 | 動作 |
| --- | --- |
| OBS に接続できない | 配信を開始しない |
| OBS が既に配信中 | 既存の配信を停止せず、開始を拒否する |
| OBS の配信先を戻せない | OBS が出力を止めるまで配信先を変更できないため、5 秒間だけ再試行する。戻せない場合は警告を残す |
| fallback 登録の設定が不完全 | 設定エラーとして保存・開始しない |
| GStreamer の Edamame 要素が不足 | 配信を開始しない |
| ImageFlux の GStreamer 要素が不足 | Edamame だけで配信を開始する |
| ImageFlux のチャンネル作成に失敗 | Edamame だけで配信を開始する |
| ImageFlux の WHIP が切れる | ImageFlux の GStreamer だけをバックオフして再起動する。Edamame は継続する |
| ImageFlux の HLS が生成されない | Edamame は継続し、画面に生成待ちを表示する。fallback 登録は失敗表示にする |
| ImageFlux の暗号鍵を取得できない | fallback 登録を中止し、GUI とログに表示する。配信は継続する |
| fallback 登録の通信または 5xx | 1 秒から 30 秒の間隔で再試行する。配信は継続する |
| fallback 登録の 400 または 401 | 再試行せず、設定エラーとして GUI とログに表示する。配信は継続する |
| ImageFlux の再接続でプレイリスト URL が変わる | 再登録しない。次回の配信開始まで古い URL のまま |
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

OBS のパスワード、ImageFlux の API トークン、`STREAM_INGEST_TOKEN` も設定ファイルへ保存する。
Unix では保存時に 0600 へ変更し、所有者だけが読めるようにする。
設定画面の「OS のアプリで開く」から、OS の既定アプリで TOML を直接編集できる。
ファイルが無い場合は、現在の設定を保存してから開く。
編集後は「再読込」を押す。

fallback 登録を使う場合は、`[stream_credentials]` に live2025-server の `base_url` と `ingest_token` を指定する。
`base_url` は視聴者と `tomyou` の双方から到達できる HTTPS のオリジンにする。
鍵 URI は `base_url` と `channel_id` から自動で決まる。
`[stream_credentials]` が空の場合は登録せず、ImageFlux の HLS は暗号化しない。

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

fallback 登録を使う場合は、上記に加えて次を確認する。

1. GUI の fallback 登録が「登録済み」になる。
2. ImageFlux のプレイリストの `#EXT-X-KEY` URI に `fallback=1` と `kid` が含まれる。
3. 通常用のプレイリストを取得できない状態で、`GET /channel/<channel>` の `live_url` が ImageFlux の URL になる。
4. セッション Cookie 付きの `GET /key/<channel>?fallback=1` が、登録した鍵を返す。

入力チェックは、1920x1080/AAC のストリームで GUI が「OK」になり、解像度や音声形式が違う場合に赤くなることを確認する。
`cargo test -p tomyou` では、ffmpeg が使える環境で 1920x1080/AAC の結合テストも実行する。

ローカル RTMP の遅延は、初回のキーフレーム待ち、再接続、HLS の遅延と分けて測る。

## 制約

- 録画、映像プレビュー、遠隔操作、複数チャンネル同時配信は行わない。
- 映像の自動再エンコード、自前 WebRTC、ImageFlux の RTMP ingest は行わない。
- `tomyou` と `whip-script` を同じ OBS に対して同時に動かさない。
- 入力チェックは表示だけで、期待値と違っても配信を止めない。
- fallback 登録は配信開始後に一度だけ行う。ImageFlux の再接続でプレイリスト URL が変わった場合は、次回の配信開始まで再登録しない。
- fallback 行を削除する API はない。7 日を超える連続配信では鮮度切れになる。
- Edamame と ImageFlux の独立した冗長化ではない。
