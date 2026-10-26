# publisher

publisher は映像からセグメントファイルを作り、controller に `PUT` する送信側プログラムである。
付属の `edamame-publisher` は SRT または RTMP の入力を受け取り、ffmpeg で MPEG-TS に変換して AES-128-CBC で暗号化する。
プレイリストは送信せず、生成と配信先 worker の選択は controller に任せる。
MP4 を使う検証には、暗号化しない [検証用 publisher](#検証用-publisher) を使える。

## 導入前に確認すること

- 暗号化：`input.key_uri` を省略しても暗号化する。
  省略時はプレイリストに `#EXT-X-KEY` が出ないため、通常の HLS プレイヤーでは再生できない。
- 鍵配信：通常の再生には `key_uri` を指定し、使用中の 16 バイトの鍵を配信するサーバーを別途用意する。
  controller と worker は鍵を保存も配信もしない。
  鍵の URL は視聴者から到達できる必要がある。
- ブラウザの認証と CORS：別 origin の鍵を取得する場合は、鍵配信側の CORS と、プレイヤーが必要な cookie を送れるかを確認する。
  cookie を使う構成では資格情報付きリクエストの許可、cookie の属性、ブラウザの制約も満たす必要があり、controller / worker の CORS 設定だけでは解決しない。
- HTTPS：HTTPS のページから再生する場合は、プレイリスト、セグメント、鍵の取得先もブラウザに許可される scheme と証明書を用意する。
- 鍵のログ：起動時には使用する鍵の全体を hex 形式で標準出力に出す。
  `--key-file` を指定しても鍵の hex 出力は止まらない。
  収集したログも秘密情報として扱い、閲覧権限と保存期間を制限し、問い合わせや公開ログへそのまま貼り付けない。

## 必要な環境

- Linux など、Unix 向けの publisher を実行できる環境
- SRT と RTMP、使用するエンコーダに対応した ffmpeg と ffprobe
- SRT または RTMP を送る映像送信側。
  音声がある場合は AAC を使う
- 起動済みの [controller](./controller.md) と、再生まで確認する場合は [worker](./worker.md)
- 通常の HLS 再生を行う場合は、使用中の鍵を配信するサーバー

GPU は常に必須ではない。
ソフトウェアエンコードには `libx264` を使い、入力が条件を満たす場合は `mode = "copy"` で映像を再エンコードせずに配信できる。
GPU なしで動かす構成では、全体の `encoder.codec` も `libx264` にする。
`h264_qsv` を指定すると入力のデコードにも QSV を使うため、copy の出力だけでも GPU が必要になる。

- NVEnc を使う場合は NVIDIA の GPU とドライバが必要になる。
  同時エンコード数の制限は GPU とドライバの仕様を確認する
- QSV を使う場合は Intel の GPU、VA-API ドライバ、oneVPL の GPU ランタイムが必要になる。
  Ubuntu 26.04 では `intel-media-va-driver-non-free` と `libmfx-gen1.2` を使う

GPU を利用できるかは、実際に使う環境で確認する。

```bash
ffmpeg -hide_banner -loglevel error -f lavfi -i testsrc2=size=1920x1080:rate=30 -t 5 \
  -c:v h264_nvenc -f null -
```

QSV の場合は次のコマンドを使う。

```bash
vainfo --display drm --device /dev/dri/renderD128
ffmpeg -hide_banner -loglevel error -f lavfi -i testsrc2=size=1920x1080:rate=30 -t 5 \
  -c:v h264_qsv -f null -
```

## ビルドと最小起動

リポジトリのルートでビルドする。

```bash
cargo build --release --locked -p edamame-publisher
```

[controller](./controller.md) と [worker](./worker.md) を起動してから、入力と鍵配信の設定を用意する。
以下は 1 入力、1 画質をソフトウェアエンコードする完全な TOML の例であり、`publisher.toml` として保存する。
`key_uri` は例示用 URL なので、実際に鍵を配信する URL に置き換える。
この設定だけで鍵配信サーバーが起動したり、再生可能になったりするわけではない。

```toml
segment_duration_ms = 6000
backlog_limit = 6

[encoder]
codec = "libx264"
preset = "veryfast"

[[input]]
channel_id = "live"
input_url = "srt://0.0.0.0:9101?mode=listener&latency=200000"
key_uri = "https://keys.example.test/live/key"

[[input.rendition]]
quality = "720p"
mode = "encode"
resolution = "1280x720"
bandwidth = 3500000
video_bitrate = 3000000
```

次のコマンドは controller がローカルで起動済みで、同じ認証トークンを使う前提である。
`publisher-key.bin` は新規に生成される鍵、または既存の 16 バイトの生データであり、鍵配信側にも同じ値を設定する。
鍵ファイルの親ディレクトリは先に用意する。
入力側は `9101` 番ポートへ SRT で映像を送る。

```bash
./target/release/edamame-publisher \
  --config publisher.toml \
  --controller-url http://127.0.0.1:9000 \
  --auth-token example-token \
  --key-file ./publisher-key.bin
```

この例では外部サービスへの自動登録を使わないため、登録しない旨の警告が出る。
鍵配信は別途設定する必要がある。

[複数入力の設定例](../crates/edamame-publisher/publisher.example.toml) は GPU、複数 channel、外部の鍵配信 URL を前提とする。
そのままローカルで再生できる最小設定ではないため、入力、エンコーダ、配信 URL、鍵の設定を環境に合わせる。

## 設定リファレンス

### TOML

設定ファイルの 1 本の `[[input]]` が 1 つの channel に対応し、その中の `[[input.rendition]]` が quality を表す。

| 項目 | 必須条件と説明 |
| --- | --- |
| `segment_duration_ms` | 必須。全 channel 共通のセグメント長をミリ秒で指定する。500 以上。PUT の `duration_ms` にもこの値を使う |
| `backlog_limit` | 必須。正の整数。未送信セットを保持する期限は `segment_duration_ms × backlog_limit` ミリ秒 |
| `encoder.codec` | 必須。ffmpeg の映像エンコーダ。`h264_nvenc`、`h264_qsv`、`libx264` など |
| `encoder.preset` | 必須。エンコーダに対応するプリセット。NVEnc は `p1` から `p7`、QSV は `veryfast` から `veryslow` など |
| `input.channel_id` | 必須。`[A-Za-z0-9_-]` の 1 から 64 文字。同じ設定内で重複不可 |
| `input.input_url` | 必須。`srt://` または `rtmp://`。SRT は URL の `mode` で接続方式を指定し、RTMP は publisher が待ち受ける。RTMP の app と stream は送信側とそろえる |
| `input.live_url` | 任意。公開マスタープレイリストの HTTPS URL。`--stream-credentials-url` を使う場合は全 input で必須 |
| `input.key_uri` | 任意。鍵を取得する HTTP または HTTPS の絶対 URL。通常の HLS プレイヤーで再生する場合は必要 |
| `input.rendition.quality` | 必須。quality の識別子。同じ channel 内で重複不可。`master` は使用不可 |
| `input.rendition.mode` | 任意。省略時は `encode`。`copy` は映像を再エンコードしない。音声はどちらもコピーする |
| `input.rendition.resolution` | 必須。`WIDTHxHEIGHT`。幅と高さはそれぞれ 2 以上の偶数。encode はこの解像度へ変換し、copy は入力との一致を検証する |
| `input.rendition.bandwidth` | 必須。正の整数。controller へ宣言する bit/s。音声と多重化を含めて見積もる |
| `input.rendition.video_bitrate` | encode の場合は正の整数が必須。copy の場合は指定不可 |

`copy` では入力映像が H.264 であり、キーフレームの間隔がセグメント長を割り切る必要がある。
6 秒セグメントなら 1 秒、2 秒、3 秒、6 秒の間隔が使える。
割り切れないと長いセグメントと短いセグメントが混ざる。

### CLI

CLI オプションを `EDAMAME_` 系の環境変数へ自動変換する機能は publisher にはない。
接続先や認証情報は CLI で指定する。

| option | 必須条件と説明 |
| --- | --- |
| `--config` | 必須。TOML 設定ファイルのパス |
| `--controller-url` | 必須。HTTP または HTTPS の絶対 URL。query と fragment は不可 |
| `--auth-token` | CLI では任意だが、controller の PUT 認証に必要。同じ token を設定し、省略時は 401 になる |
| `--work-dir` | 任意。作業ディレクトリを作る親ディレクトリ。配下に `edamame-publisher-<pid>` を作る。省略時は一時ディレクトリを使う |
| `--key-file` | 任意。16 バイトの生データの鍵ファイル。存在すれば読み、なければ権限 0600 で生成する。省略時は実行ごとに鍵を生成する |
| `--stream-credentials-url` | 任意。連携する `live2025-server` のベース URL。暗号鍵と `live_url` を登録する |
| `--stream-ingest-token` | `--stream-credentials-url` と組で指定する。外部サービスの `STREAM_INGEST_TOKEN` と同じ Bearer token |

### 外部サービスへの登録

`--stream-credentials-url` を指定すると、channel ごとに `POST /internal/stream-credentials` へ `channel_id`、`live_url`、hex 形式の `encryption_key` を送る。
全 input の `live_url` と、`--stream-ingest-token` が必要になる。
登録の完了を待たずにセグメントの送信を始めるため、PUT の成功だけでは鍵の登録成功を判断しない。
外部サービスの鍵配信と公開 URL の設定は別途確認する。

## Docker での起動

[Dockerfile](../Dockerfile) の `publisher` ターゲットは ffmpeg と Intel の GPU ドライバを含む。

```bash
docker build --target publisher -t edamame-publisher .
```

Linux ホストで、ソフトウェアエンコードする設定を使う例を示す。
鍵のファイルと作業用の保存先には、他の利用者が読めない権限を設定する。
この例の鍵はコンテナの一時領域ではなく、ホスト側の `publisher-state` に保存する。

```bash
mkdir -m 700 -p publisher-state
docker run --rm \
  --network host \
  -v "$PWD/publisher.toml:/etc/edamame-publisher/publisher.toml:ro" \
  -v "$PWD/publisher-state:/var/lib/edamame-publisher" \
  edamame-publisher \
  --config /etc/edamame-publisher/publisher.toml \
  --controller-url http://127.0.0.1:9000 \
  --auth-token example-token \
  --key-file /var/lib/edamame-publisher/key.bin
```

NVEnc では NVIDIA Container Toolkit を用意して `--gpus all` を追加し、QSV では `--device /dev/dri:/dev/dri` を追加する。
SRT は UDP、RTMP は TCP を使う。
host ネットワークを使わない場合は入力用ポートを対応するプロトコルで公開し、controller への到達先もコンテナから接続できる URL に変える。

## 動作と運用上の注意点

### セグメントと暗号鍵

全 quality で同じ番号のセグメントがそろったものを 1 セットとし、共通のランダムな識別子を付けて送る。
一部の quality がそろわないセットは ffmpeg の実行終了時に捨てる。
送信済みのセグメントファイルは作業領域から削除する。

- 鍵はプロセス内の全 channel で共通に使う。
  `--key-file` を指定すれば再起動後も同じ鍵を使う
- `key_uri` がある場合はセグメントごとに乱数 IV を作り、PUT に鍵 URI と IV を付ける
- `key_uri` がない場合は全ゼロの固定 IV で暗号化し、鍵 URI と IV を PUT しない。
  暗号化なしの配信にはならない
- 平文セグメントは権限 0700 の作業ディレクトリに一時保存する。
  正常終了時は削除し、登録失敗などで異常終了した場合は残ることがある

鍵を変更すると、同じ鍵 URI をキャッシュしているプレイヤーは新しい鍵を取り直すまで復号できないことがある。
鍵の更新と視聴中の再生をどう扱うかは、鍵配信側でも確認する。

### 再起動と時刻

ffmpeg は実行ごとに出力の先頭時刻を 0 に寄せるため、publisher は送信済みセット数とセグメント長から `-output_ts_offset` を計算する。
publisher 自体の起動時は controller のプレイリストから次のセグメント番号を推定し、取得できなければ `0` を使う。
これは出力時刻の初期値であり、controller の採番を指定するものではない。
外部配信へリダイレクトされる場合や、controller が状態を失った場合は、再起動前後の時刻の連続性を保証しない。

バックログの破棄や PUT の失敗で一部のセグメントが欠けると、実際の再生では欠けた区間が飛ぶことがある。
再起動後の採番と状態消失の扱いは [controller](./controller.md) と [冗長化](./cluster.md) を参照する。

### 入力の検査

ffmpeg の実行ごとの最初のセットで、音声があれば AAC であることを検証する。
copy では映像の H.264 と解像度も検証し、条件を満たさない場合はその channel を止めて待ち受けを再開する。
copy の入力がインターレースの場合は警告を出す。

確定したセグメント長が設定値から 100 ms を超えてずれる場合も警告を出す。
警告後も controller へ宣言する `duration_ms` は設定値のままであるため、送信側のキーフレーム間隔などを直す。
QSV は対応する入力を GPU でデコードし、失敗時にソフトウェアデコードへは切り替わらない。
NVEnc と libx264 は入力をソフトウェアでデコードするため、CPU 使用量にも余裕を持たせる。

## 動作確認

controller と worker の準備が終わるまでは、プレイリストが 404 または 503 になることがある。
設定例の channel と quality に対し、プレイリストを取得する。

```bash
curl --fail --show-error --retry 30 --retry-delay 1 \
  'http://127.0.0.1:9000/channels/live/master.m3u8?external=0'
curl --fail --show-error --retry 30 --retry-delay 1 \
  http://127.0.0.1:9000/channels/live/720p.m3u8
```

最初の PUT 前の 404 はこの `--retry` では再試行されないため、入力と PUT のログを確認してから取得し直す。
メディアプレイリストに出た実際のセグメント URL と、その直前の `#EXT-X-KEY` の鍵 URI と IV を確認する。
固定の `0.ts` が保持されているとは限らないため、現在のプレイリストにない URL を使って確認しない。

手動で復号を確認する場合は、`SEGMENT_URL` にプレイリストの URL、`KEY_HEX` に対応する鍵の hex、`IV` に対応する IV の先頭の `0x` を除いた値を設定して実行する。
この操作は秘密情報を扱い、鍵をコマンド引数に渡すため、共有環境でのプロセス一覧やログの閲覧権限にも注意する。

```bash
curl --fail --show-error --output segment.ts "$SEGMENT_URL"
openssl enc -d -aes-128-cbc \
  -K "$KEY_HEX" -iv "$IV" \
  -in segment.ts -out plain.ts
ffprobe plain.ts
```

`key_uri` なしの送信を診断する場合だけ全ゼロの IV を使う。
復号と ffprobe の成功は、ブラウザが鍵を取得して継続再生できることの証明にはならない。
鍵 URI に必要な cookie を含めて実際のプレイヤーから取得できることと、映像の解像度、再生時刻の進行を [再生確認](./operations.md#再生を確認する) に従って確認する。
複数 quality では全画質を確認する。

## 失敗時の扱い

| 事象 | 扱い |
| --- | --- |
| ffmpeg の終了、SRT / RTMP の切断 | 1 秒から 10 秒の待ち時間を置いて起動し直す。セグメントを作れていた実行では待ち時間を 1 秒に戻す |
| PUT の接続エラー、5xx、成功応答の読み取り失敗 | 同じセグメントを 1 秒間隔で送り直す。セットの保持期限を超えれば捨てる |
| PUT の 400、401、409 | 設定の誤りとみなし、その channel を止める。待ち時間を置いて起動し直す |
| 鍵登録時の接続エラー、400 / 401 以外の失敗応答 | 1 秒から 30 秒の待ち時間を置いて登録を試し直す。セグメントの配信は続ける |
| 鍵登録時の 400、401 | ログを出して登録を打ち切る。セグメントの配信は続ける |
| 未送信セットが `segment_duration_ms × backlog_limit` ミリ秒を超える | セットを捨てる |
| AAC 以外の音声、copy の前提を満たさない入力 | その channel を止めて待ち受けを再開する。他の channel は続ける |
| SIGINT、SIGTERM | ffmpeg を停止して終了する |

channel が止まってもプロセス全体は終了せず、送信側が設定を直して接続し直せば再開できる。
鍵登録に成功しないまま終了した場合は終了コード `1` になり、作業ディレクトリを残す。
設定の検証や鍵ファイルの読み込みなど、起動時の失敗も終了コード `1` になる。

## 検証用 publisher

[loop-publisher.py](../scripts/loop-publisher.py) は、MP4 を無限ループさせて controller へ PUT し続ける検証用スクリプトである。
再エンコードも暗号化もせず、素材のキーフレーム位置でセグメントを切り出す。
暗号化なしの配信経路を確認したい場合は、ライブ入力と鍵配信を必要とする `edamame-publisher` の代わりに使える。

### 必要な環境と素材

Python 3、ffmpeg、ffprobe と、起動済みの [controller](./controller.md) と [worker](./worker.md) が必要になる。
素材は先頭からキーフレームを持ち、キーフレーム間隔がそろい、セグメント長の倍数の位置にキーフレームがあることが必要である。
素材の長さもセグメント長の倍数にする。
起動時に ffprobe で検査し、満たさない素材はエラーにする。
キーフレームの位置と素材の長さには、フレーム単位の量子化を考慮して 1 フレーム以内のずれを許容する。

6 秒セグメントの 720p 素材を、音声付きの MP4 から作る例を示す。
`input.mp4` を実際の入力ファイルに置き換える。

```bash
ffmpeg -i input.mp4 \
  -map 0:v:0 -map 0:a:0 \
  -vf scale=1280:720 -r 30 \
  -c:v libx264 -preset ultrafast -tune zerolatency -pix_fmt yuv420p \
  -b:v 3000000 -maxrate 3000000 -bufsize 6000000 \
  -force_key_frames 'expr:gte(t,n_forced*6)' -g 180 -sc_threshold 0 \
  -c:a aac -b:a 128k -ar 48000 -ac 2 \
  -t 630 \
  source-720p.mp4
```

`-t` はセグメント長の倍数にする。
上の例の 630 秒より入力が短い場合は、入力の長さ以下で最大の 6 の倍数を指定する。
音声付き素材では AAC のプリミングによる負の PTS が残り、ループの継ぎ目のセグメントは素材の端数とプリミングの分だけ長くなることがある。
48 kHz の AAC のプリミングは約 21 ms である。

### 起動とオプション

以下はリポジトリのルートで実行する。
同じ channel の既存 quality とセグメント長や bandwidth が異なる場合は衝突するため、検証対象の設定をそろえるか、別の channel を使う。

```bash
python3 scripts/loop-publisher.py \
  --source ./source-720p.mp4 \
  --controller-url http://127.0.0.1:9000 \
  --channel-id live \
  --quality 720p \
  --bandwidth 3500000 \
  --segment-duration-ms 6000 \
  --resolution 1280x720 \
  --auth-token example-token
```

| option | 必須条件と説明 |
| --- | --- |
| `--source` | 必須。入力 MP4 のパス |
| `--controller-url` | 必須。controller の HTTP の絶対 URL。HTTPS は初期接続先に指定できず、query と fragment は不可 |
| `--channel-id` | 必須。channel の識別子 |
| `--quality` | 必須。quality の識別子 |
| `--bandwidth` | 必須。正の整数。音声と多重化を含めた bit/s を宣言する |
| `--segment-duration-ms` | 必須。500 以上のミリ秒。素材の 1 フレーム以上にし、キーフレーム間隔と素材の長さに合わせる |
| `--resolution` | 任意。宣言する解像度を `WIDTHxHEIGHT` で指定する |
| `--auth-token` | CLI では任意だが、controller の PUT 認証に必要。同じ token を指定する |
| `--external-m3u8` | 任意。外部配信のマスタープレイリスト URL。最初のセグメントの PUT が成功した後に controller へ登録する |

セグメント識別子は毎回ランダムに生成し、controller が割り当てた `sequence` を標準出力へ記録する。
配信の確認は [再生確認](./operations.md#再生を確認する) に従う。
このスクリプトには `edamame-publisher` の鍵配信設定は不要である。

### 再起動と失敗時の扱い

起動時は `master.m3u8?external=0` から channel 全体の次のセグメント番号を推定し、出力時刻の offset に使う。
master が 404 なら新しい channel として offset `0` で開始する。
master の 422 / 503 と media の 404 / 422 / 503 は、0.5 秒間隔で最大 60 回取得を試みる。
これらの再試行でも取得できない場合や、接続失敗など他の取得エラーが起きた場合は、ffmpeg を起動する前に終了する。
controller の実際の採番は controller に任せ、offset では指定しない。

再起動すると素材の内容は先頭に戻るが、時刻はプレイリストから推定した続きになる。
量子化、素材の端数、音声のプリミングにより、再起動後の切り位置と長さは連続実行時と前後にずれることがある。
直前のセグメントと時刻が重なったり空いたりするが、このずれが再起動のたびに積み上がるわけではない。

PUT の接続失敗や応答の破損では、同じ識別子とファイルを 1 秒間隔で最大 3 回送る。
失敗応答や再試行の上限に達すると、セグメントを残して再送コマンドを表示し、終了する。
認証を使う場合は再送用の設定ファイルに token を保存するため、そのファイルも秘密情報として扱い、再送後は削除する。
スクリプトは `edamame-publisher` のような channel の自動再起動を行わない。
