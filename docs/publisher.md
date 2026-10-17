# publisher

この文書では、付属のエンコーダおよび送信プロセスである `crates/edamame-publisher` の導入手順、設定、内部動作、および障害時の扱いを説明する。
全体構成と用語の定義は [docs/README.md](./README.md) で説明する。
MP4 ファイルを無限ループさせてテスト配信を行うスクリプトは [scripts/README.md](../scripts/README.md) で説明する。

`edamame-publisher` は、SRT または RTMP で受け取ったライブ映像を `ffmpeg` でエンコードして MPEG-TS（`.ts`）のセグメントファイルを作り、AES-128-CBC で暗号化して Edamame の `controller` へ `PUT` し続ける。
設定ファイル内の 1 本の入力（`[[input]]`）が 1 つの `channel` に対応し、1 つの `channel` は複数の `quality`（設定上の `[[input.rendition]]`）を持つ。

`controller` から見ると `edamame-publisher` は HTTP API を呼ぶクライアントの 1 つであり、プレイリストの生成や `worker` の管理には関与しない。
外部の配信管理および鍵配布サーバ（`live2025-server`）を使う環境では、`--stream-credentials-url` を指定すると、生成した AES-128 暗号鍵と公開マスタープレイリスト URL（`live_url`）を `POST /internal/stream-credentials` へ自動登録する。

## 必要な環境

- NVIDIA の GPU とドライバ（NVEnc を使う場合）。同時に使える NVEnc セッション数は、ドライバ 550.54.14 以降では 8 までである
- Intel の GPU と VA-API のドライバ、oneVPL の GPU ランタイム（QSV を使う場合）。Ubuntu 26.04 では `intel-media-va-driver-non-free` と `libmfx-gen1.2` を入れる（oneVPL の本体は `ffmpeg` が依存する `libvpl2` で入る）
- SRT と RTMP に対応した `ffmpeg` と `ffprobe`（Ubuntu の `ffmpeg` パッケージは両方を満たす）
- 起動済みの Edamame `controller`（プレイリストの取得と再生まで確認する場合は `worker` も起動しておく）
- SRT または RTMP を送ってくる映像送信側

NVIDIA の GPU で NVEnc が使えるかは、次のコマンドで確かめる。

```bash
ffmpeg -hide_banner -loglevel error -f lavfi -i testsrc2=size=1920x1080:rate=30 -t 5 \
  -c:v h264_nvenc -f null -
```

Intel の GPU で QSV が使えるかは、次のコマンドで確かめる。

```bash
vainfo --display drm --device /dev/dri/renderD128
ffmpeg -hide_banner -loglevel error -f lavfi -i testsrc2=size=1920x1080:rate=30 -t 5 \
  -c:v h264_qsv -f null -
```

## ビルド

```bash
cargo build --release -p edamame-publisher
```

Docker イメージを作る場合は、このリポジトリの Dockerfile にある `publisher` ターゲットを指定する（`controller` や `worker` の distroless イメージとは異なり、`ffmpeg` と Intel の GPU ドライバを含む）。

```bash
docker build --target publisher -t edamame-publisher .
```

## 設定

### 設定ファイル（TOML）

`crates/edamame-publisher/publisher.example.toml` をコピーし、`channel_id`、`input_url`、および各 `quality` の設定を環境に合わせる。

| 項目 | 説明 |
| --- | --- |
| `segment_duration_ms` | 全 `channel` 共通のセグメント長（500 以上）。セグメントの `PUT` に付ける `duration_ms` にもこの値を使う |
| `backlog_limit` | `channel` ごとに未送信セットを保持する時間を `segment_duration_ms` の倍数で指定する。この時間を過ぎたセットは捨てる |
| `encoder.codec` | `ffmpeg` の映像エンコーダ。NVIDIA の GPU では `h264_nvenc`、Intel の GPU では `h264_qsv` を推奨し、GPU のない検証では `libx264` にする |
| `encoder.preset` | `ffmpeg` のプリセット（NVEnc は `p1` から `p7`、QSV は `veryfast` から `veryslow`） |
| `input.channel_id` | `channel` の識別子（`[A-Za-z0-9_-]` の 1 から 64 文字） |
| `input.input_url` | 入力の URL。`srt://` は URL の `mode` で待ち受け、`rtmp://` は `publisher` が待ち受ける。RTMP では URL の app と stream が送信側と一致していること |
| `input.live_url` | `controller` の公開マスタープレイリストの HTTPS URL。`--stream-credentials-url` で鍵と一緒に登録する場合は必須 |
| `input.key_uri` | メディアプレイリストに出力する暗号鍵の URI。指定すると `PUT` に `key_uri` と `iv` を付ける |
| `rendition.quality` | `quality` の識別子。同じ `channel` 内で同じ値は重複させない。`master` は予約されているため使えない |
| `rendition.mode` | `copy` は入力の映像ストリームをそのままコピーする。省略時（`encode`）は指定したエンコーダで再エンコードする。音声はどちらのモードでも再エンコードせずコピーする（`-c:a copy`）ため、入力音声は AAC である必要がある |
| `rendition.resolution` | `WIDTHxHEIGHT`（幅と高さはどちらも偶数）。再エンコード時はこの解像度へリサイズし、`copy` のときは入力映像の解像度と一致するかを検証する |
| `rendition.bandwidth` | `controller` へ宣言するビットレート。音声と多重化を含めた値にする |
| `rendition.video_bitrate` | エンコーダの目標ビットレート。`mode = "copy"` のときは指定しない |

`copy` を使う `quality` では、入力映像が H.264 であり、キーフレームの間隔がセグメント長を割り切ることが必要である。
たとえばセグメント長が 6 秒なら、1 秒、2 秒、3 秒、6 秒の間隔が使える。
`ffmpeg` の HLS muxer は実行の先頭からセグメント長の倍数の位置で区切るため、割り切れないと長いセグメントと短いセグメントが混ざる。
確定したセグメント長が指定値から 100 ms を超えてずれた場合は `publisher` が警告を出すため、送信側のキーフレーム間隔を直す。

### CLI オプション

実行環境ごとに変わる接続先や認証情報は CLI オプションで渡す。

| option | 説明 |
| --- | --- |
| `--config` | TOML 設定ファイルのパス（必須） |
| `--controller-url` | `controller` の接続先を表す `http` または `https` の絶対 URL（必須） |
| `--auth-token` | `controller` の `--auth-token` と同じトークン。省略すると `PUT` に `Authorization` ヘッダを付けない |
| `--work-dir` | 作業ディレクトリを作る親ディレクトリ。指定したディレクトリの配下に `edamame-publisher-<pid>` を作る。省略した場合は一時ディレクトリの配下に作る |
| `--key-file` | 暗号鍵を読み書きするファイル。ファイルがあれば 16 byte の生データとして読み、なければ生成して権限 0600 で保存する。省略すると実行ごとに生成する |
| `--stream-credentials-url` | 外部の配信管理および鍵配布サーバ（`live2025-server`）のベース URL。指定すると暗号鍵と `live_url` を登録する |
| `--stream-ingest-token` | `live2025-server` の `STREAM_INGEST_TOKEN` と同じ Bearer トークン |

`--stream-credentials-url` を指定した場合は、`--stream-ingest-token` とすべての input の `live_url` が必要になる。
指定しない場合は登録を行わず、起動時に警告を出す。

## 起動

### バイナリで起動する

先に `controller` を起動しておく。

```bash
edamame-controller \
  --bind 0.0.0.0:9000 \
  --storage-capacity 512MiB \
  --playlist-segments 6 \
  --sync-interval-ms 1000 \
  --sync-timeout-ms 10000 \
  --publish-wait-ms 2000 \
  --auth-token example-token
```

続けて `edamame-publisher` を起動する。

```bash
./target/release/edamame-publisher \
  --config publisher.toml \
  --controller-url http://127.0.0.1:9000 \
  --auth-token example-token \
  --key-file /var/lib/edamame-publisher/key.bin \
  --stream-credentials-url https://live.example.test \
  --stream-ingest-token example-ingest-token
```

起動すると、使用する暗号鍵を hex 形式で標準出力に出す。
`--key-file` を省略した場合、鍵は起動のたびに変わり、ファイルには保存されない。

```text
encryption key file is /var/lib/edamame-publisher/key.bin
encryption key (hex) is 0123456789abcdef0123456789abcdef
segment duration is 6000 ms
work directory is /tmp/edamame-publisher-1234
channel=1A input_url=srt://0.0.0.0:9101?mode=listener&latency=200000 renditions=3
channel=1A starting ffmpeg run=1 offset_ms=0
channel=1A registered stream credentials
channel=1A published index=0 quality=1080p identifier=... sequence=0
```

### Docker で起動する

NVIDIA の GPU は NVIDIA Container Toolkit（`--gpus all`）で、Intel の GPU は `--device /dev/dri:/dev/dri` でコンテナに割り当てる。

```bash
docker run --rm --gpus all \
  --network host \
  -v "$PWD/publisher.toml:/etc/edamame-publisher/publisher.toml:ro" \
  edamame-publisher \
  --config /etc/edamame-publisher/publisher.toml \
  --controller-url http://127.0.0.1:9000 \
  --auth-token example-token
```

Intel の GPU を使う場合は `--gpus all` の代わりに `--device /dev/dri:/dev/dri` を渡す。

```bash
docker run --rm --device /dev/dri:/dev/dri \
  --network host \
  -v "$PWD/publisher.toml:/etc/edamame-publisher/publisher.toml:ro" \
  edamame-publisher \
  --config /etc/edamame-publisher/publisher.toml \
  --controller-url https://edamame-controller.example.test \
  --auth-token example-token
```

SRT は UDP で届くため、コンテナの待ち受けポートを公開するか `--network host` を使う。
RTMP は TCP で届くため、待ち受けポートを公開すれば `--network host` は不要である。

## 動作を確認する

あらかじめ `worker`（例：`http://127.0.0.1:9001`、起動手順は [運用](./operations.md) を参照）を起動しておく。
`controller` のプレイリストを取得する。`worker` がセグメントを取得するまで、`controller` は `503` を返すことがある。

```bash
curl -i --retry 30 --retry-delay 1 --retry-connrefused http://127.0.0.1:9000/channels/1A/master.m3u8
curl -i http://127.0.0.1:9000/channels/1A/1080p.m3u8
```

`worker` からセグメントファイルを取得し、鍵で復号して MPEG-TS として読めることを確かめる。
`key_uri` を設定している場合は、プレイリストの `#EXT-X-KEY` に書かれた `IV` を使う。

```bash
curl -o segment.ts http://127.0.0.1:9001/segments/1A/1080p/0.ts
openssl enc -d -aes-128-cbc \
  -K 0123456789abcdef0123456789abcdef \
  -iv "$IV" \
  -in segment.ts -out plain.ts
ffprobe plain.ts
```

`$IV` には、対象セグメントの `#EXT-X-KEY` にある `IV` 属性から先頭の `0x` を除いた値を設定する。
`input.key_uri` を指定していない場合はプレイリストに `IV` 属性が出ないため、全ゼロの IV を使う。

`--stream-credentials-url` を使っている場合は、鍵と配信 URL の登録を確かめる。
`GET /channel/:channel_id` の `live_url` と、`GET /key/:channel_id` が返す鍵を確認する。
鍵のエンドポイントはセッション cookie を要求するため、実際の確認は `client` と同じ経路で行う。

```bash
curl -i 'https://live.example.test/channel/uni'
curl -i -b 'session=<session>' 'https://live.example.test/key/uni' | xxd
```

## 内部動作の仕様

### 処理の流れ

1. 設定ファイルと CLI オプションを読み込み、識別子や URL を検証する
2. 16 byte の暗号鍵を用意し（`--key-file` があれば読み書きする）、hex で標準出力に出す
3. `channel` ごとに、`controller` のプレイリストから次のセグメント番号を求める。取得できない場合は `0` から始める
4. `channel` ごとに、暗号鍵と `live_url` を `live2025-server` へ登録する。セグメントの配信は登録の完了を待たない
5. `channel` ごとに `ffmpeg` を起動する。1 入力 N 出力とし、`quality` ごとに HLS muxer でセグメントファイルを作る
6. 全 `quality` で同じ番号のセグメントが揃ったものを 1 セットとして扱い、共通のセグメント識別子を付けて送信キューに入れる
7. セグメントを AES-128-CBC で暗号化し、`quality` ごとに `controller` へ `PUT` する
8. `PUT` が終わったセグメントファイルを削除する

`publisher` は `channel` ごとに、エンコード用、送信用、登録用のスレッドを持つ。
エンコード側は `ffmpeg` の起動と監視、セグメントの収集を担当する。
送信側は暗号化、`PUT`、再試行を担当し、登録側は `live2025-server` への登録を担当する。
`channel` の間で共有するのは設定と暗号鍵だけであり、キューや `ffmpeg` プロセス、登録処理は共有しない。

`ffmpeg` の HLS muxer は、`-hls_flags temp_file` により書き込みが終わったセグメントファイルだけを最終的な名前で保存する。
`publisher` は `run-<n>/<quality>/<index>.ts` を走査し、全 `quality` のセグメントが揃った最小の番号から順に送る。
揃わなかった番号のセグメントは、その `ffmpeg` の実行が終わった時点で捨てる。
セグメント識別子はセットごとに 16 byte の乱数から作り、同じセットの全 `quality` で同じ値を使う。

`run-<n>` の番号は `channel` ごとに数え、作業ディレクトリに残っている最大の番号の続きから使う。
前の実行が残したセグメントを新しい実行が読み直して、同じ原因で再び停止するのを防ぐためである。

### 暗号化

`edamame-publisher` は `input.key_uri` の有無にかかわらず、常にセグメントファイルを AES-128-CBC（PKCS7 パディング）で暗号化してから `PUT` する。

- 鍵は `publisher` プロセス全体で 1 つ用意し、全 `channel` で共通に使う。`--key-file` を指定した場合はそのファイルに保存し、再起動後も同じ鍵を使う
- `input.key_uri` を指定した input では、セグメントごとに乱数 IV を作り、`PUT` に `key_uri` と `iv` を付けて `controller` に伝える。`controller` はメディアプレイリストの `#EXT-X-KEY` に暗号鍵の URI と IV を出力し、再生側は `key_uri` から鍵を取得して復号する
- `input.key_uri` を指定しない input では、全ゼロ固定の IV で暗号化し、`PUT` には `key_uri` も `iv` も付けない。プレイリストに `#EXT-X-KEY` が出力されないため、通常の HLS プレイヤーで再生する場合は `input.key_uri` を指定する
- 暗号化前の平文セグメントは、作業ディレクトリ（権限 0700）に一時的に置かれる。作業ディレクトリは正常終了時に削除し、鍵の登録が成功しないまま終了した場合だけ残す

### 時刻の扱い

`ffmpeg` は実行ごとに出力の先頭時刻を 0 に寄せるため、そのまま再起動するとセグメントのタイムスタンプが巻き戻る。
そこで `publisher` は、それまでに `PUT` したセット数に `segment_duration_ms` を掛けた値を `-output_ts_offset` に渡し、再起動しても再生側の時刻の並びが続くようにする。

起動時は `controller` のプレイリストから次のセグメント番号を求める。
`publisher` 自体を再起動した場合も、この番号を使って時刻を続ける。
プレイリストを取得できない場合は `0` から始める。

再生側のプレイリストは設定した `duration_ms` の並びで作られる。
そのため、バックログの破棄や `PUT` の失敗で一部のセグメントが欠けても時刻は詰まらず、実際の再生では欠けた区間が飛ぶ。

### 検査

`ffmpeg` の実行ごとに、最初のセットで次を確かめる。

- 音声がある場合は AAC であること。AAC 以外ならその `channel` を止め、待ち時間を置いて起動し直す
- `copy` の `quality` では、映像が H.264 であり、解像度が設定と一致すること。満たさない場合はその `channel` を止め、待ち時間を置いて起動し直す
- `copy` の `quality` では、`ffprobe` の `field_order` が progressive と unknown のどちらでもなければ警告を出す

セグメントの長さは HLS muxer が書いたプレイリストの `#EXTINF` から読み取り、次のセグメントが現れた時点で直前のセグメントの区切りが確定したと判断する。
区切りが確定したセグメントの長さが `segment_duration_ms` から 100 ms を超えてずれている場合は警告を出す。
警告のあとも、`publisher` は `duration_ms` に設定値をそのまま使って配信を続ける。

## 失敗時の扱いと制限

### 失敗時の扱い

| 事象 | 扱い |
| --- | --- |
| ffmpeg の終了 | 1 秒から 10 秒の待ち時間を置いて起動し直す。終了した ffmpeg がセグメントを作れていた場合は、待ち時間を 1 秒に戻す |
| SRT や RTMP の切断 | ffmpeg の終了と同じ扱いとする。送信側が接続し直せば配信を再開する |
| 接続エラー、5xx、応答の読み取り失敗 | 同じセグメントを 1 秒間隔で送り直す |
| 400、401、409 | 設定の誤りとみなし、その channel を止める。待ち時間を置いて起動し直す |
| 鍵の登録時の接続エラー、5xx | 1 秒から 30 秒の待ち時間を置いて登録を試し直す。配信は続ける |
| 鍵の登録時の 400、401 | 設定の誤りとみなし、ログを出して登録を打ち切る。配信は続ける |
| 未送信セットが `segment_duration_ms` × `backlog_limit` の時間を過ぎる | そのセットを捨てる |
| AAC 以外の音声、`copy` の前提を満たさない入力 | その channel を止める。待ち時間を置いて起動し直し、他の channel は続ける |
| SIGINT、SIGTERM | 全 ffmpeg を止めて終了する |

`channel` が止まった場合、`publisher` はその `channel` の `ffmpeg` と送信処理を終わらせ、他の `channel` の配信を続ける。
止まった `channel` も待ち受けを再開するため、送信側が設定を直して接続し直せば配信を再開できる。
`publisher` プロセス自体は終了しない。
鍵の登録に成功しないまま `publisher` が終了した場合に限り、終了コードは `1` になり、作業ディレクトリを残す。

### 制限

- `input.key_uri` を指定しない場合は IV が全ゼロ固定になり、プレイリストにも `#EXT-X-KEY` が出力されない
- `live2025-server` は `channel` ごとに 1 つの鍵を持つため、`publisher` を再起動して鍵を変えると、視聴中の再生は鍵を取り直すまで止まる
- `copy` の `quality` は、入力のキーフレーム間隔がセグメント長を割り切る場合だけセグメントの長さが揃う
- `h264_qsv` では入力の映像コーデックを QSV がデコードできる必要がある。デコードできない入力では `ffmpeg` が起動に失敗し、ソフトウェアデコードへは切り替わらない
- `h264_nvenc` と `libx264` では入力の映像をソフトウェアでデコードする。CPU が足りない場合は NVDEC の利用を検討する
