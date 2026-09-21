# 運用

## 構成

Edamame は 1 つの controller と 1 つ以上の worker で構成する。
publisher は controller にセグメントファイルを PUT する。
client は controller からプレイリストを取得し、worker からセグメントファイルを取得する。

```
publisher -- PUT セグメントファイル --> controller
client ---- GET プレイリスト --> controller
client ---- GET セグメントファイル ---> worker
controller <-- sync/GET ---- worker
```

client は外部の HLS player であり、Edamame の CLI option では設定しない。
client には、controller に到達できる URL を設定する。

## controller の設定

環境変数の名前は、option 名の `-` を `_` に変えて `EDAMAME_` を付けたものにする。
各 option はコマンドライン引数か環境変数で指定する。
両方を指定した場合はコマンドライン引数を優先する。

| option | 環境変数 | 必須 | 説明 |
| --- | --- | --- | --- |
| `--bind` | `EDAMAME_BIND` | 必須 | HTTP server が待ち受けるアドレスであり、client や worker が接続する URL ではない |
| `--storage-capacity` | `EDAMAME_STORAGE_CAPACITY` | 必須 | 正規セグメントファイルをメモリ上に保存する容量（`256MiB` のような 2 進接頭辞付きの値、または byte 数。1 以上） |
| `--playlist-segments` | `EDAMAME_PLAYLIST_SEGMENTS` | 必須 | メディアプレイリストに含める直近のセグメント数（全 channel に共通、6 以上の整数） |
| `--sync-interval-ms` | `EDAMAME_SYNC_INTERVAL_MS` | 必須 | worker に指示する sync の間隔をミリ秒で指定する（1 以上の整数） |
| `--sync-timeout-ms` | `EDAMAME_SYNC_TIMEOUT_MS` | 必須 | sync が届かない worker を配信先から外すまでの待ち時間をミリ秒で指定する（1 以上の整数） |
| `--publish-wait-ms` | `EDAMAME_PUBLISH_WAIT_MS` | 任意 | 新しいセグメントを公開する前に worker の取得を待つ時間をミリ秒で指定する（0 以上の整数、省略時は 2000 で警告を出す） |
| `--auth-token` | `EDAMAME_AUTH_TOKEN` | 任意 | PUT の認証に使うトークン（空白と制御文字を除く表示可能 ASCII、省略すると PUT を認証せず、起動時に警告を出す） |
| `--cors-origin` | `EDAMAME_CORS_ORIGIN` | 任意 | ブラウザからの GET を許可する origin（省略すると CORS を付けず、起動時に警告を出す） |
| `--external-min-percent` | `EDAMAME_EXTERNAL_MIN_PERCENT` | 任意 | 外部配信へ流す最低の割合（%、0 以上 100 以下、省略時は 1） |

## worker の設定

| option | 環境変数 | 必須 | 説明 |
| --- | --- | --- | --- |
| `--worker-id` | `EDAMAME_WORKER_ID` | 必須 | controller に登録する worker の識別子 |
| `--bind` | `EDAMAME_BIND` | 必須 | HTTP server が待ち受けるアドレス |
| `--public-url` | `EDAMAME_PUBLIC_URL` | 必須 | client に公開する worker の URL であり、`http` または `https` で始まる絶対 URL（TLS 終端の reverse proxy を置く場合は `https`） |
| `--controller-url` | `EDAMAME_CONTROLLER_URL` | 必須 | worker から controller に接続するための、`http` の絶対 URL |
| `--cache-limit` | `EDAMAME_CACHE_LIMIT` | 必須 | `(channel_id, quality)` ごとのキャッシュ上限（1 以上の整数） |
| `--weight` | `EDAMAME_WEIGHT` | 任意 | worker の重み（1 以上の整数、省略時は 1） |
| `--auth-token` | `EDAMAME_AUTH_TOKEN` | 任意 | controller の `--auth-token` と同じトークン（省略すると登録の PUT に `Authorization` ヘッダーを付けない） |
| `--cors-origin` | `EDAMAME_CORS_ORIGIN` | 任意 | ブラウザからの GET を許可する origin（省略すると CORS を付けず、起動時に警告を出す） |

`--weight`、`--auth-token`、`--cors-origin` 以外の option は必須で、既定値はない。
`--public-url` は `--bind` から推測しない。
`0.0.0.0` など client から到達できないアドレスで待ち受ける場合でも、`--public-url` には client から到達できる URL を指定する。
`--controller-url` には、worker から到達できる `http` の絶対 URL だけを指定できる。
query と fragment は含めず、末尾の `/` は取り除いて使う。

## 起動と確認

controller を起動する。

```bash
edamame-controller \
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
edamame-worker \
  --worker-id worker-a \
  --bind 127.0.0.1:9001 \
  --public-url http://127.0.0.1:9001 \
  --controller-url http://127.0.0.1:9000 \
  --cache-limit 12 \
  --auth-token example-token \
  --cors-origin '*'
```

セグメントファイルを PUT する。

```bash
printf 'segment-1' > /tmp/segment-1.ts
curl -i -X PUT \
  -H 'Authorization: Bearer example-token' \
  --data-binary @/tmp/segment-1.ts \
  'http://127.0.0.1:9000/segments/live/720p/segment-1?duration_ms=6000&bandwidth=3000000&resolution=1280x720'
```

レスポンスの `sequence` が controller の割り当てたセグメント番号である。
この例では 0 になる。

セグメントを AES-128 で暗号化する場合は、鍵を指す `key_uri` を付けて PUT する。
鍵の実体は publisher が配信し、controller は鍵を取得も検証もしない。

```bash
curl -i -X PUT \
  -H 'Authorization: Bearer example-token' \
  --data-binary @/tmp/segment-1.ts \
  'http://127.0.0.1:9000/segments/live/720p/segment-1?duration_ms=6000&bandwidth=3000000&resolution=1280x720&key_uri=https%3A%2F%2Fkeys.example.test%2Flive%2Fkey-1'
```

`key_uri` は `http` または `https` の絶対 URL とする。
暗号鍵の URI はセグメントごとに記録されるため、続くセグメントに同じ鍵を使う場合も、そのセグメントの PUT で同じ `key_uri` を指定する。
暗号鍵の URI を変えた PUT を送ると、そのセグメントのプレイリストに新しい暗号鍵の URI が反映される。
暗号鍵の URI を付けない PUT は、そのセグメントだけを暗号化なしに戻す。
client はプレイリストの `#EXT-X-KEY` に書かれた暗号鍵の URI から自分で鍵を取得するため、鍵の配信元は client から到達できるようにする。

worker は次の sync でセグメントファイルを取得する。
最初の公開範囲ができるまで、メディアプレイリストは 503 を返すことがある。
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

## sync とタイムアウト

worker は登録時に受け取った `sync_interval_ms` の間隔で controller に sync を送る。
controller は `--sync-timeout-ms` を超えて sync が届かない worker を配信先の候補から外す。

`--sync-timeout-ms` は `--sync-interval-ms` の 3 倍以上を推奨する。
1 回や 2 回の sync が遅れても配信先から外さないためである。

新しいセグメントファイルを PUT してから、controller がプレイリストに反映するまでには公開待ちがある。
controller は最初の未公開セグメントファイルを PUT した時刻から `--publish-wait-ms` だけ待つ。
追加の PUT では待ち時間を延長しない。
公開待ちの間は、未公開セグメントを公開範囲の候補にもフォールバックにも含めない。
公開待ちが終わった時点で存在するセグメントを公開可能として扱う。

controller は sync を受けた時に、各 quality の公開範囲を決める。
sync が届いている worker が連続してキャッシュしている公開可能範囲のうち、最新のものを選ぶ。
公開範囲は quality ごとに独立して決まり、quality 間で一致させる必要はない。
media sequence は前進するだけでなく、後退したり飛んだりすることがある。
現在の公開範囲を保持している worker がいれば、より新しい候補が出るまで維持する。
現在の公開範囲を保持している worker がいなければ、次の sync で、より古い範囲も含めて配信可能な最新の候補へ更新する。
更新するまでの間、プレイリストのリクエストには 503 を返すことがある。
公開範囲の切り替えは sync の処理で行い、プレイリストのリクエストでは行わない。

公開範囲が変わるまで、プレイリストは前の公開範囲を返す。
初回など公開範囲がまだない場合と、公開範囲を保持する worker がいない quality は、次の sync で公開範囲が更新されるまで 503 になる。
`--publish-wait-ms` の推奨値は `--sync-interval-ms` の 2 倍とする。
1 回目の sync で取得し、次の sync で報告する時間を確保するためである。
`--sync-interval-ms` はセグメントの `duration_ms` より十分小さくする。

worker のキャッシュ上限（`--cache-limit`）は controller の `--playlist-segments` より 1 以上大きくすることを推奨する。
公開待ちの間は前の公開範囲と新しい範囲の両方を保持できるため、公開待ちの途中で古い範囲へフォールバックしにくくなる。
キャッシュ上限が小さい場合、controller は次の sync で保持されている最新の連続範囲へ切り替える。

## worker の選択

controller はメディアプレイリストを生成するときに、quality ごとに、公開範囲をキャッシュしている worker から配信先を選ぶ。
候補が複数ある場合は、登録された重みの比に従って選ぶ。
重み 4 の worker と重み 1 の worker があれば、5 回のうち 4 回は重み 4 の worker を選ぶ。

重みは相対値であり、選ばれる回数の比になる。
同じスペックのホストなら同じ値にし、速いホストほど大きい値にする。
例えば 4 vCPU のホストを `--weight 1`、16 vCPU のホストを `--weight 4` にする。
`--weight` を省略した worker は 1 として扱う。

重みを変えたときと、配信先の候補となる worker が増減したときは、その時点から重みの比に従って選び直す。
controller は worker の負荷情報（CPU 使用率、同時接続数、配信量）を扱わない。
そのため、一時的に性能が落ちた worker でも重みの分だけ選ばれる。

登録時の `weight` は 1 以上の整数とする。
0 を送ると controller は 400 を返す。

## 容量

controller はセグメントファイルをメモリ上に保存する。
保存しているバイト数の合計が `--storage-capacity` を超えたときは、書き込んだ順に古いセグメントファイルから削除する。
容量はセグメントファイルのバイト数の合計であり、セグメント識別子や暗号化の指定などのメタデータの分は含まない。
メタデータは実行中に PUT したセグメント数に比例して増える。

容量は、公開範囲と、公開待ちと sync 間隔の間に worker が取得する範囲を保持できる大きさにする。
容量が足りないと公開範囲のセグメントファイルが削除され、その quality のメディアプレイリストは 422 を返す。
セグメントファイル 1 つが容量より大きい場合、その PUT には 413 を返し、保存しない。

目安の計算式は、保持したいセグメント数 × 1 セットの最大サイズ × channel 数とする。
保持したいセグメント数は、`--playlist-segments` に公開待ちと sync 間隔の分を足した数より大きくする。

publisher の設定例（6 秒セグメント、1080p 12 Mbps、720p 6.5 Mbps、480p 1.4 Mbps）では、3 quality 合計で 1 セットあたり約 14 MiB になる。
`--playlist-segments 6` に公開待ちと sync 間隔の分を足した 8 セグメントを下回らないよう、余裕を見て 10 セグメント分を 3 channel で保持すると仮定すると、約 14 MiB × 10 × 3 ≒ 430 MiB になる。
この構成では `--storage-capacity 512MiB` を目安にする。
セグメント長、ビットレート、channel 数、quality 数が大きい場合はその分を増やす。

worker のキャッシュ上限は `(channel_id, quality)` ごとのセグメント数の上限であり、worker 全体の容量上限ではない。
worker はセグメントファイルをメモリ上にキャッシュし、上限を超えた古いものから削除する。

## 再起動

controller を再起動すると、メモリにあるセグメントファイル、channel、quality、セグメントのメタデータ、セグメント識別子とセグメント番号の対応、worker の登録はすべて失われる。
セグメント番号は 0 から始まり、新しい識別子に順に割り当てる。
`GET /segments/...` は再起動後に PUT されるまで 404 を返す。
プレイリストは再起動後に PUT されたセグメントファイルから作り直すため、publisher がセグメントファイルを PUT するまで 404 になる。
worker は sync で 404 を受け取ると再登録し、登録に成功した時点でキャッシュを捨てて取り直す。
取得と sync を再開すれば配信先に戻る。
client はプレイリストを取り直し、メディアシーケンスが 0 から始まるプレイリストに合わせて再同期する。

メタデータは最初に成功した PUT の属性から作り直す。
同じ識別子を再 PUT すると新しい番号で保存される。

worker を再起動すると、メモリ上のキャッシュは失われる。
worker は controller に登録し、取得を指示されたセグメントファイルを取り直す。
controller が停止中の間は、client は worker が保持しているセグメントファイルだけを取得できる。

worker はセグメントファイルを保存するたびに、`(channel_id, quality)` ごとにキャッシュ上限まで新しい順に残す。
上限を超えた古いセグメントファイルは削除する。

起動の順序は問わない。
worker は controller への登録に失敗しても動作を続け、1 秒後に再試行する。
登録に成功するまでは controller から間隔を受け取れないため、1 秒は固定の再試行間隔とする。
登録に成功すると、受け取った間隔で sync を送る。

## 失敗時の扱い

| 状況 | 動作 |
| --- | --- |
| controller が停止した | client はプレイリストを取得できない。worker はキャッシュしているセグメントファイルの配信を続けるが、sync は失敗し続ける。controller が復帰すると、worker は再登録してキャッシュを捨てる |
| worker が停止した | controller は `--sync-timeout-ms` の経過後にその worker を配信先の候補から外す。その quality の公開範囲を保持する他の worker があればそれを使い、なければ次の sync で公開範囲が更新されるまで 503 になる |
| セグメントファイルの取得に失敗した | worker はログを出して次の sync で同じセグメントファイルを再取得する。公開範囲を保持していなければ配信先に使われない |
| 公開範囲のセグメントファイルが容量超過で削除された | controller はその quality のプレイリストを生成せず 422 を返し、worker がキャッシュしているセグメントファイルの配信は続く |
| 保存容量より大きいセグメントを PUT した | controller は 413 を返し、セグメントファイルを保存せず、channel の属性もセグメント番号も確定しない |
| 容量超過で削除された quality に同じ識別子を再 PUT した | 別の quality に識別子が残っていれば同じセグメント番号に保存し直す。古いバイト列をキャッシュしているか、取得中の worker には次の sync で再取得を指示する |
| worker のトークンが controller と一致しない | controller は登録の PUT に 401 を返し、worker はログを出して 1 秒後に再試行する |
| controller がメタデータを失った | セグメント番号は 0 から始まり、publisher がセグメントファイルを PUT するまでプレイリストは 404 になる。worker は登録時にキャッシュを捨てて取り直す |
| controller の再起動後に同じ識別子を再 PUT した | 新しい番号を割り当てて保存する。前の実行のセグメントファイルは残っていない |
| client が古いプレイリストを持つ | プレイリストに書かれた worker が停止している場合、セグメントファイルの取得は失敗する。controller は `--sync-timeout-ms` の経過後にその worker を候補から外すまで同じ worker を選ぶことがある（候補が複数台ある場合は、重みに従って他の worker も選ばれる） |
| 暗号鍵の配信元が停止した | client は鍵を取得できず、その鍵を使うセグメントを復号できない。controller と worker は鍵を保持しないため、鍵を取得できるかどうかは publisher の配信元に依存する |
| 公開範囲に暗号鍵の URI の異なるセグメントが混ざった | メディアプレイリストはセグメントごとに `#EXT-X-KEY` を出し分ける。client はセグメントごとに適切な鍵を取得する |
| 外部配信が停止した | controller は外部の状態を確認しないため、外部配信の登録を取り消すか、controller を再起動するまで振り分けを続ける |
| controller が再起動した | 登録された外部配信は失われ、publisher が再登録するまで外部へ振り分けない |

## セキュリティと信頼境界

- controller は `--auth-token` を指定すると PUT に Bearer トークンを要求し、指定しなければ警告を出して PUT を認証しない
- publisher が送るセグメント識別子は文字種を限定し、controller と worker は識別子をメモリ上の索引にだけ使う
- worker が取得する URL は `--controller-url` とセグメントの識別子から作り、外部から渡された URL は取得しない
- `--public-url` には client から到達できる URL を指定し、controller はこの URL をプレイリストに書く
- controller は登録された外部配信の URL を取得しない。client が外部配信から直接取得する
- 外部配信への振り分けは、接続元の IP と channel から決まる値と、その時点の公開の遅れに応じた外部配信の割合とを比較し、リクエストごとに判定する。割合が変われば、同じ接続元と channel でも判定結果が変わることがある。プロキシや NAT 配下で接続元の IP が同じ client は、同じ channel と割合であれば同じ判定結果になる
- CORS は GET の読み取りだけを許可し、`PUT` と `POST` には CORS ヘッダを付けない

## ログ

- ログメッセージは英語とする
- 起動時に待ち受けアドレスを出力する
- sync の失敗とセグメントファイルの取得失敗は標準エラーに出力する
- メトリクス用のエンドポイントは提供しない
