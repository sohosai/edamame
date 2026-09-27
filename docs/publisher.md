# publisher

この文書は `crates/edamame-publisher` の導入手順、処理の流れ、運用上の仕様を定める。

SRT または RTMP で受け取った映像をエンコードして MPEG-TS のセグメントを作り、AES-128 で暗号化して edamame controller へ PUT し続ける。
1 本の入力が 1 つの channel になり、1 つの channel は複数の quality（rendition）を持つ。
controller から見ると publisher は HTTP API を使う外部ソフトウェアであり、プレイリストの生成や worker の管理には関与しない。
鍵は publisher が作り、live2025-server の `POST /internal/stream-credentials` で配信 URL と一緒に登録する。

## 必要な環境

- NVIDIA の GPU とドライバ（NVEnc を使う場合）。同時に使う NVEnc セッション数は、driver 550.54.14 以降では 8 までである
- Intel の GPU と VA-API のドライバ、oneVPL の GPU ランタイム（QSV を使う場合）。
  Ubuntu 26.04 では intel-media-va-driver-non-free と libmfx-gen1.2 を入れる。oneVPL の本体は ffmpeg が依存する libvpl2 で入る
- SRT と RTMP を扱える ffmpeg と ffprobe（Ubuntu の ffmpeg パッケージは両方を満たす）
- 起動済みの edamame controller
- SRT または RTMP を送ってくる送信側

GPU 機で NVEnc が使えるかは、次のコマンドで確かめる。

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

## 設定

`crates/edamame-publisher/publisher.example.toml` をコピーし、`channel_id`、`input_url`、quality の設定を環境に合わせる。
実行ごとに変わる値（controller の URL と認証トークン）は CLI で渡す。

| 項目 | 説明 |
| --- | --- |
| `segment_duration_ms` | 全 channel 共通のセグメント長（500 以上）。PUT の `duration_ms` にもこの値を使う |
| `backlog_limit` | channel ごとに未送信セットを保持する時間を `segment_duration_ms` の倍数で指定する。この時間を過ぎたセットは捨てる |
| `encoder.codec` | ffmpeg の映像エンコーダ。NVIDIA の GPU では `h264_nvenc`、Intel の GPU では `h264_qsv` を推奨し、GPU のない検証では `libx264` にする |
| `encoder.preset` | ffmpeg のプリセット（NVEnc は `p1` から `p7`、QSV は `veryfast` から `veryslow`） |
| `input.channel_id` | channel の識別子（`[A-Za-z0-9_-]` の 1 から 64 文字） |
| `input.input_url` | 入力の URL。`srt://` は URL の `mode` で待ち受け、`rtmp://` は publisher が待ち受ける。RTMP では URL の app と stream が送信側と一致していること |
| `input.live_url` | controller の公開マスタープレイリストの HTTPS URL。鍵と一緒に登録する。登録を使う場合は必須 |
| `input.key_uri` | メディアプレイリストに出力する暗号鍵の URI。指定すると PUT に `key_uri` と `iv` を付ける |
| `rendition.quality` | quality の識別子。channel の中では同じ値を使い回さない。`master` は使えない |
| `rendition.mode` | `copy` は入力の映像をそのまま使う。省略すると再エンコードする |
| `rendition.resolution` | `WIDTHxHEIGHT`。偶数であること。`copy` のときは入力の解像度と一致していること |
| `rendition.bandwidth` | controller へ宣言するビットレート。音声と多重化を含めた値にする |
| `rendition.video_bitrate` | エンコーダの目標ビットレート。`mode = "copy"` のときは指定しない |

`input.key_uri` を指定した場合、publisher はセグメントごとに乱数 IV を作り、暗号化に使った IV を PUT で controller に伝える。
伝えた IV はメディアプレイリストの `#EXT-X-KEY` に出力されるため、再生側は同じ IV で復号できる。
`input.key_uri` を指定しない場合は controller を通じて IV を伝えられないため、全ゼロ固定の IV を使う。

`copy` を使う quality では、入力が H.264 で、キーフレームの間隔がセグメント長を割り切ることが必要である。
たとえばセグメント長が 6 秒なら、1 秒、2 秒、3 秒、6 秒の間隔は使える。
ffmpeg の HLS muxer は実行の先頭からセグメント長の倍数の位置で区切るため、割り切れないと長いセグメントと短いセグメントが混ざる。
指定値から 100 ms を超えてずれた場合は警告が出るので、送信側のキーフレーム間隔を直す。

実行ごとに変わる値は CLI で渡す。

| option | 説明 |
| --- | --- |
| `--config` | TOML 設定ファイルのパス（必須） |
| `--controller-url` | controller の接続先を表す `http` または `https` の絶対 URL（必須） |
| `--auth-token` | controller の `--auth-token` と同じトークン。省略すると PUT に `Authorization` ヘッダーを付けない |
| `--work-dir` | 作業ディレクトリを作る親ディレクトリ。指定したディレクトリの配下に `edamame-publisher-<pid>` を作る。省略した場合は一時ディレクトリの配下に作る |
| `--key-file` | 暗号鍵を読み書きするファイル。ファイルがあれば 16 byte の生データとして読み、なければ生成して 0600 で保存する。省略すると実行ごとに生成する |
| `--stream-credentials-url` | live2025-server のベース URL。指定すると鍵と配信 URL を登録する |
| `--stream-ingest-token` | live2025-server の `STREAM_INGEST_TOKEN` と同じトークン |

`--stream-credentials-url` を指定した場合は `--stream-ingest-token` と全 input の `live_url` が必要になる。
指定しない場合は登録を行わず、起動時に警告を出す。

## 起動

controller の起動例を示す。

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

publisher を起動する。

```bash
./target/release/edamame-publisher \
  --config publisher.toml \
  --controller-url http://127.0.0.1:9000 \
  --auth-token example-token \
  --key-file /var/lib/edamame-publisher/key.bin \
  --stream-credentials-url https://live.example.test \
  --stream-ingest-token example-ingest-token
```

起動すると、使う暗号鍵を hex で標準出力に出す。
`--key-file` を省略した場合、鍵は実行ごとに変わり、ファイルには保存しない。

```
encryption key file is /var/lib/edamame-publisher/key.bin
encryption key (hex) is 0123456789abcdef0123456789abcdef
segment duration is 6000 ms
work directory is /tmp/edamame-publisher-1234
channel=1A input_url=srt://0.0.0.0:9101?mode=listener&latency=200000 renditions=3
channel=1A starting ffmpeg run=1 offset_ms=0
channel=1A registered stream credentials
channel=1A published index=0 quality=1080p identifier=... sequence=0
```

## 動作を確認する

controller のプレイリストを取得する。
worker がセグメントを取得するまで、controller は 503 を返すことがある。

```bash
curl -i --retry 30 --retry-delay 1 --retry-connrefused http://127.0.0.1:9000/channels/1A/master.m3u8
curl -i http://127.0.0.1:9000/channels/1A/1080p.m3u8
```

worker からセグメントファイルを取得し、鍵で復号して MPEG-TS として読めることを確かめる。
`key_uri` を設定している場合、プレイリストの `#EXT-X-KEY` に書かれた `IV` を使う。

```bash
curl -o segment.ts http://127.0.0.1:9001/segments/1A/1080p/0.ts
openssl enc -d -aes-128-cbc \
  -K 0123456789abcdef0123456789abcdef \
  -iv "$IV" \
  -in segment.ts -out plain.ts
ffprobe plain.ts
```

`$IV` には、取得するセグメントに適用されるプレイリストの `#EXT-X-KEY` の `IV` 属性から `0x` を除いた値を設定しておく。
`input.key_uri` を指定していない場合はプレイリストに `IV` 属性が出ないため、全ゼロの IV を使う。

鍵と配信 URL の登録を確かめる。
`GET /channel/:channel_id` の `live_url` と、`GET /key/:channel_id` が返す鍵を見る。
鍵のエンドポイントはセッション cookie を要求するため、実際の確認は client と同じ経路で行う。

```bash
curl -i 'https://live.example.test/channel/uni'
curl -i -b 'session=<session>' 'https://live.example.test/key/uni' | xxd
```

## 処理の流れ

1. 設定ファイルと CLI を読み、識別子や URL を検証する
2. 16 byte の暗号鍵を用意し（`--key-file` があれば読み書きする）、hex で標準出力に出す
3. channel ごとに、controller のマスタープレイリストから次のセグメント番号を求める。取得できない場合は 0 から始める
4. channel ごとに、鍵と `live_url` を live2025-server へ登録する。配信は登録の完了を待たない
5. channel ごとに ffmpeg を起動する。1 入力 N 出力とし、quality ごとに HLS muxer でセグメントファイルを作る
6. 全 quality で同じ番号のセグメントが揃ったものを 1 セットとして扱い、セグメント識別子を付けてキューに入れる
7. セグメントを AES-128-CBC で暗号化し、quality ごとに controller へ PUT する
8. PUT が終わったセグメントファイルを削除する

publisher は channel ごとに、エンコード、送信、登録の 3 本のスレッドを持つ。
エンコード側は ffmpeg の起動と監視、セグメントの収集を担当する。
送信側は暗号化、PUT、再試行を担当し、登録側は live2025-server への登録を担当する。
channel の間で共有するのは設定と暗号鍵だけであり、キューや ffmpeg、登録は共有しない。

ffmpeg の HLS muxer は、`-hls_flags temp_file` により書き込みが終わったセグメントファイルだけを最終的な名前で保存する。
publisher は `run-<n>/<quality>/<index>.ts` を走査し、全 quality のセグメントが揃った最小の番号から順に送る。
揃わなかった番号のセグメントは、ffmpeg の実行が終わった時点で捨てる。
セグメント識別子はセットごとに 16 byte の乱数から作り、同じセットの全 quality で同じ値を使う。

`run-<n>` の番号は channel ごとに数え、作業ディレクトリに残っている最大の番号の続きから使う。
前の実行が残したセグメントを新しい実行が読み直すと、止まる原因になった入力でまた止まるためである。

## 暗号化

- 方式は AES-128-CBC と PKCS7 で、セグメントファイル全体を暗号化する
- `input.key_uri` を指定した input ではセグメントごとに乱数 IV を作る。指定しない場合だけ全ゼロ固定の IV を使う
- 鍵は publisher で 1 つ用意し、全 channel で使う。`--key-file` を指定した場合はそのファイルに保存し、再起動後も同じ鍵を使う
- セグメントは暗号化する前の状態で作業ディレクトリに一時的に置かれる。作業ディレクトリは 0700 で作り、正常終了時に削除する。鍵の登録が成功しないまま終了した場合は残す

`input.key_uri` を指定した input では、PUT に `key_uri` と `iv` を付け、暗号化に使った IV を controller に伝える。
controller はメディアプレイリストの `#EXT-X-KEY` に暗号鍵の URI と IV を出力する。
再生側は `key_uri` から鍵を取得し、プレイリストに書かれた IV で復号する。

## 時刻の扱い

ffmpeg は実行ごとに出力の先頭を 0 に寄せるため、そのまま再起動するとセグメントの時刻が巻き戻る。
publisher は `-output_ts_offset` に「それまでに PUT したセット数 × `segment_duration_ms`」を渡し、再起動しても再生側の時刻の並びが続くようにする。

起動時は controller のプレイリストから次のセグメント番号を求め、publisher 自体の再起動でも同じように時刻を続ける。
プレイリストを取得できない場合は 0 から始める。

バックログの破棄や PUT の失敗で一部のセグメントが欠けた場合、その区間の時刻は詰まらない。
再生側のプレイリストは設定した `duration_ms` の並びで作られるため、実際の再生では欠けた区間が飛ぶ。

## 検査

ffmpeg の実行ごとに、最初のセットで次を確かめる。

- 音声がある場合は aac であること。aac 以外ならその channel を止め、待ち時間を置いて起動し直す
- `copy` の quality は、映像が H.264 で、解像度が設定と一致すること。違えばその channel を止め、待ち時間を置いて起動し直す
- `copy` の quality は、ffprobe の `field_order` が progressive でも unknown でもない場合だけ警告を出す。unknown は警告の対象外とする

セグメントの長さは HLS muxer が書いたプレイリストの `#EXTINF` から読み、次のセグメントが現れた時点で直前のセグメントの区切りが確定したと判断する。
区切りが確定したセグメントの長さが `segment_duration_ms` から 100 ms を超えてずれている場合は警告を出す。
警告のあとも `duration_ms` は設定値のまま配信を続ける。

## 失敗時の扱い

| 事象 | 扱い |
| --- | --- |
| ffmpeg の終了 | 1 秒から 10 秒の待ち時間を置いて起動し直す。終了した ffmpeg がセグメントを作れていた場合は、待ち時間を 1 秒に戻す |
| SRT や RTMP の切断 | ffmpeg の終了と同じ扱いとする。送信側が接続し直せば配信を再開する |
| 接続エラー、5xx、応答の読み取り失敗 | 同じセグメントを 1 秒間隔で送り直す |
| 400、401、409 | 設定の誤りとみなし、その channel を止める。待ち時間を置いて起動し直す |
| 鍵の登録時の接続エラー、5xx | 1 秒から 30 秒の待ち時間を置いて登録を試し直す。配信は続ける |
| 鍵の登録時の 400、401 | 設定の誤りとみなし、ログを出して登録を打ち切る。配信は続ける |
| 未送信セットが `segment_duration_ms` × `backlog_limit` の時間を過ぎる | そのセットを捨てる |
| aac 以外の音声、`copy` の前提を満たさない入力 | その channel を止める。待ち時間を置いて起動し直し、他の channel は続ける |
| SIGINT、SIGTERM | 全 ffmpeg を止めて終了する |

channel が止まった場合、publisher はその channel の ffmpeg と送信を終わらせ、他の channel の配信を続ける。
止まった channel は待ち受けを再開するため、送信側が設定を直して接続し直せば配信を再開できる。
止まった channel はプロセスの終了対象にしない。
鍵の登録が成功しないまま終了した場合だけ、プロセスの終了コードは 1 になり、作業ディレクトリを残す。

## Docker で起動する

このリポジトリの Dockerfile にある `publisher` ターゲットでイメージを作る。
controller と worker のイメージ（distroless）と違い、ffmpeg と Intel のドライバを含む。
NVIDIA の GPU は NVIDIA Container Toolkit で渡し、Intel の GPU は `--device /dev/dri:/dev/dri` で渡す。

```bash
docker build --target publisher -t edamame-publisher .
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

SRT は UDP で届くため、待ち受けるポートを開けるか `--network host` を使う。
RTMP は TCP で届くため、公開するポートを指定すれば `--network host` は要らない。

## 制限

- `input.key_uri` を指定しない場合は IV が全ゼロ固定になり、セグメントごとに変えられない
- live2025-server は channel ごとに 1 つの鍵を持つため、publisher を再起動して鍵を変えると、視聴中の再生は鍵を取り直すまで止まる
- `copy` の quality は、入力のキーフレーム間隔がセグメント長を割り切る場合だけセグメントの長さが揃う
- `h264_qsv` では入力の映像コーデックを QSV がデコードできる必要がある。デコードできない入力では ffmpeg が起動に失敗し、ソフトウェアデコードへは切り替わらない
- `h264_nvenc` と `libx264` では入力の映像をソフトウェアでデコードする。CPU が足りない場合は NVDEC の利用を検討する
