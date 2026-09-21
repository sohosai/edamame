# publisher

この文書は `crates/edamame-publisher` の導入手順、処理の流れ、運用上の契約を定める。

SRT で受け取った映像をエンコードして MPEG-TS のセグメントを作り、AES-128 で暗号化して edamame controller へ PUT し続ける。
1 本の入力が 1 つの channel になり、1 つの channel は複数の quality（rendition）を持つ。
controller から見ると publisher は HTTP API を使う外部ソフトウェアであり、プレイリストの生成や worker の管理には関与しない。
鍵は publisher が作り、live2025-server の `POST /internal/stream-credentials` で配信 URL と一緒に登録する。

## 必要な環境

- NVIDIA の GPU とドライバ（NVEnc を使う場合）。同時に使う NVEnc セッション数は、driver 550.54.14 以降では 8 までである
- SRT を扱える ffmpeg と ffprobe（Ubuntu の ffmpeg パッケージは両方を満たす）
- 起動済みの edamame controller
- SRT を送ってくる送信側

GPU 機で NVEnc が使えるかは、次のコマンドで確かめる。

```bash
ffmpeg -hide_banner -loglevel error -f lavfi -i testsrc2=size=1920x1080:rate=30 -t 5 \
  -c:v h264_nvenc -f null -
```

## ビルド

```bash
cargo build --release -p edamame-publisher
```

## 設定

`crates/edamame-publisher/publisher.example.toml` をコピーし、`channel_id`、`srt_url`、rendition を環境に合わせる。
実行ごとに変わる値（controller の URL と認証トークン）は CLI で渡す。

| 項目 | 説明 |
| --- | --- |
| `segment_duration_ms` | 全 channel 共通のセグメント長（500 以上）。PUT の `duration_ms` にもこの値を使う |
| `backlog_limit` | channel ごとの未送信セグメントの上限。`segment_duration_ms` を掛けた時間を過ぎたセグメントは捨てる |
| `encoder.codec` | ffmpeg の映像エンコーダ。`h264_nvenc` を推奨し、GPU のない検証では `libx264` にする |
| `encoder.preset` | ffmpeg のプリセット（NVEnc は `p1` から `p7`） |
| `input.channel_id` | channel の識別子（`[A-Za-z0-9_-]` の 1 から 64 文字、`master` は使えない） |
| `input.srt_url` | 入力の SRT URL。`mode=listener` で待ち受ける |
| `input.live_url` | controller の公開マスタープレイリストの HTTPS URL。鍵と一緒に登録する。登録を使う場合は必須 |
| `input.key_uri` | メディアプレイリストに出す鍵 URI。指定すると PUT に `key_uri` と `iv` を付ける |
| `rendition.quality` | quality の識別子。channel の中では同じ値を使い回さない |
| `rendition.mode` | `copy` は入力の映像をそのまま使う。省略すると再エンコードする |
| `rendition.resolution` | `WIDTHxHEIGHT`。偶数であること。`copy` のときは入力の解像度と一致していること |
| `rendition.bandwidth` | controller へ宣言するビットレート。音声と多重化を含めた値にする |
| `rendition.video_bitrate` | エンコーダの目標ビットレート。`mode = "copy"` のときは指定しない |

`input.key_uri` を指定した場合、publisher はセグメントごとに乱数 IV を作り、暗号化に使った IV を PUT で controller に伝える。
伝えた IV は `#EXT-X-KEY` としてメディアプレイリストに出るため、再生側は同じ IV で復号できる。
指定しない場合は IV を伝える手段がないため、全ゼロ固定の IV を使う。

`copy` を使う rendition では、入力が H.264 で、キーフレームの間隔がセグメント長を割り切ることが必要である。
たとえばセグメント長が 6 秒なら、1 秒、2 秒、3 秒、6 秒の間隔は使える。
ffmpeg の HLS muxer は実行の先頭からセグメント長の倍数の位置で切るため、割り切れないと
長いセグメントと短いセグメントが混ざる。指定値から 100 ms を超えてずれた場合は警告が出るので、
送信側のキーフレーム間隔を直す。

実行ごとに変わる値は CLI で渡す。

| option | 説明 |
| --- | --- |
| `--key-file` | 暗号鍵を読み書きするファイル。ファイルがあれば 16 byte の生データとして読み、なければ生成して 0600 で保存する。省略すると実行ごとに生成する |
| `--stream-credentials-url` | live2025-server のベース URL。指定すると鍵と配信 URL を登録する |
| `--stream-ingest-token` | live2025-server の `STREAM_INGEST_TOKEN` と同じトークン |

`--stream-credentials-url` を指定した場合は `--stream-ingest-token` と全 input の `live_url` が要る。
指定しない場合は登録を行わず、起動時に警告を出す。

## 起動

controller の起動例を示す。

```bash
edamame-controller \
  --bind 0.0.0.0:9000 \
  --storage-dir /var/lib/edamame/controller \
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
channel=1A srt_url=srt://0.0.0.0:9101?mode=listener&latency=200000 renditions=3
channel=1A starting ffmpeg run=1 offset_ms=0
channel=1A registered stream credentials
channel=1A published index=0 quality=1080p identifier=... sequence=0
```

## 動作を確認する

controller のプレイリストを取得する。worker がセグメントを取得するまで 503 を返すことがある。

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
  -iv 00000000000000000000000000000000 \
  -in segment.ts -out plain.ts
ffprobe plain.ts
```

鍵と配信 URL の登録を確かめる。`GET /channel/:channel_id` の `live_url` と、`GET /key/:channel_id` が返す鍵を見る。
鍵のエンドポイントはセッション cookie を要求するため、実際の確認は client と同じ経路で行う。

```bash
curl -i 'https://live.example.test/channel/uni'
curl -i -b 'session=<session>' 'https://live.example.test/key/uni' | xxd
```

## 処理の流れ

1. 設定ファイルと CLI を読み、識別子や URL を検証する
2. 16 byte の暗号鍵を用意し（`--key-file` があれば読み書きする）、hex で標準出力に出す
3. channel ごとに、controller の master playlist から次のセグメント番号を求める。取得できない場合は 0 から始める
4. channel ごとに、鍵と `live_url` を live2025-server へ登録する。配信はこの完了を待たない
5. channel ごとに ffmpeg を起動する。1 入力 N 出力とし、rendition ごとに HLS muxer でセグメントを書く
6. 全 rendition で同じ番号のセグメントが揃ったものを 1 セットとして扱い、識別子を付けてキューに入れる
7. セグメントを AES-128-CBC で暗号化し、rendition ごとに controller へ PUT する
8. PUT が終わったセグメントファイルを削除する

channel ごとに、エンコード、送信、登録の 3 本のスレッドを持つ。エンコード側が ffmpeg の起動と監視とセグメントの収集を担当し、送信側が暗号化と PUT と再試行を担当し、登録側が live2025-server への登録を担当する。
channel の間で共有するのは設定と暗号鍵だけであり、キューや ffmpeg、登録は共有しない。

ffmpeg の HLS muxer は、`-hls_flags temp_file` により書き終えたセグメントだけを最終的な名前で置く。
publisher は `run-<n>/<quality>/<index>.ts` を走査し、全 rendition が揃った最小の番号から順に送る。揃わなかった番号は ffmpeg の実行が終わった時点で捨てる。
識別子はセットごとに 16 byte の乱数から作り、同じセットの全 rendition で同じ値を使う。

## 暗号化

- 方式は AES-128-CBC と PKCS7 で、セグメントファイル全体を暗号化する
- IV は全ゼロ固定とする
- 鍵は publisher で 1 つ用意し、全 channel で使う。`--key-file` を指定した場合はそのファイルに保存して再起動でも同じ鍵を使う
- セグメントは暗号化する前の状態で作業ディレクトリに一時的に置かれる。作業ディレクトリは 0700 で作り、終了時に削除する

`key_uri` を設定した input では、PUT に `key_uri` と `iv=00000000000000000000000000000000` を付け、
controller のメディアプレイリストに `#EXT-X-KEY:METHOD=AES-128,URI="{key_uri}",IV=0x00000000000000000000000000000000` を出してもらう。
暗号化に使った IV とプレイリストが示す IV を一致させるため、IV は固定で全ゼロとし、PUT でもその値を伝える。
再生側は `key_uri` から鍵を取り、その IV で復号する。

## 時刻の扱い

ffmpeg は実行ごとに出力の先頭を 0 に寄せるため、そのまま再起動するとセグメントの時刻が巻き戻る。
publisher は `-output_ts_offset` に「それまでに PUT したセット数 × `segment_duration_ms`」を渡し、
再起動しても再生側の時刻の並びが続くようにする。

起動時は controller の playlist から次のセグメント番号を求め、publisher 自体の再起動でも同じように時刻を続ける。
playlist を取得できない場合は 0 から始める。

バックログの破棄や PUT の失敗で一部のセグメントが欠けた場合、その区間の時刻は詰まらない。
再生側のプレイリストは設定した `duration_ms` の並びで作られるため、実際の再生では欠けた区間が飛ぶ。

## 検査

ffmpeg の実行ごとに、最初のセットで次を確かめる。

- 音声がある場合は aac であること。aac 以外ならその channel を止める
- `copy` の rendition は、映像が H.264 で、解像度が設定と一致すること。違えばその channel を止める
- `copy` の rendition が progressive でない場合は警告だけを出す

セグメントの長さは HLS muxer が書いた playlist の `#EXTINF` から読み、次のセグメントが現れた時点で「そのセグメントは切られた」と判断する。
切られたセグメントの長さが `segment_duration_ms` から 100 ms を超えてずれている場合は警告を出す。
警告のあとも `duration_ms` は設定値のまま配信を続ける。

## 失敗時の扱い

| 事象 | 扱い |
| --- | --- |
| ffmpeg の終了 | 1 秒から 10 秒の待ちで起動し直す。セグメントを作れた実行では待ちを 1 秒に戻す |
| SRT の切断 | 上と同じ。送信側が接続し直せば配信を再開する |
| 接続エラー、5xx、応答が読めない | 同じセグメントを 1 秒間隔で送り直す |
| 400、401、409 | 設定の誤りとみなし、その channel を止める |
| 鍵の登録の接続エラー、5xx | 1 秒から 30 秒の待ちで試し直す。配信は続ける |
| 鍵の登録の 400、401 | 設定の誤りとみなし、ログを出して登録を打ち切る。配信は続ける |
| 未送信セットが `backlog_limit` を超える | 古いセットから捨てる |
| aac 以外の音声、`copy` の前提違い | その channel を止める。他の channel は続ける |
| SIGINT、SIGTERM | 全 ffmpeg を止め、作業ディレクトリを削除して終了する |

channel が止まった場合、publisher はその channel の ffmpeg と送信を終わらせ、他の channel の配信を続ける。
channel が 1 つでも止まった場合と、鍵の登録が成功しないまま終わった場合、プロセスの終了コードは 1 になる。

## Docker で起動する

このリポジトリの Dockerfile にある `publisher` ターゲットでイメージを作る。
controller と worker のイメージ（distroless）と違い、ffmpeg を含む。
GPU は NVIDIA Container Toolkit で渡す。

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

`compose.yaml` にも起動例がある。SRT は UDP で届くため、待ち受けるポートを開けるか `--network host` を使う。

## 制限

- IV は全ゼロ固定であり、セグメントごとに変えられない
- live2025-server は channel ごとに 1 つの鍵を持つため、publisher を再起動して鍵を変えると、視聴中の再生は鍵を取り直すまで止まる
- `copy` の rendition は、入力のキーフレーム間隔がセグメント長を割り切る場合だけセグメントの長さが揃う
- 入力の映像はソフトウェアでデコードする。CPU が足りない場合は NVDEC の利用を検討する
