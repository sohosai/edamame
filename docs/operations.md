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
各 option はコマンドライン引数か環境変数で指定する。両方を指定した場合はコマンドライン引数を優先する。

| option | 環境変数 | 必須 | 説明 |
| --- | --- | --- | --- |
| `--bind` | `EDAMAME_BIND` | 必須 | HTTP server が待ち受けるアドレスであり、client や worker が接続する URL ではない |
| `--storage-dir` | `EDAMAME_STORAGE_DIR` | 必須 | 正規セグメントファイルを保存するディレクトリ |
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
| `--controller-url` | `EDAMAME_CONTROLLER_URL` | 必須 | controller に接続するための、worker から到達できる URL |
| `--cache-dir` | `EDAMAME_CACHE_DIR` | 必須 | セグメントファイルのキャッシュ先ディレクトリ |
| `--cache-limit` | `EDAMAME_CACHE_LIMIT` | 必須 | `(channel_id, quality)` ごとのキャッシュ上限（1 以上の整数） |
| `--weight` | `EDAMAME_WEIGHT` | 任意 | worker の相対的な配信能力（1 以上の整数、省略時は 1） |
| `--auth-token` | `EDAMAME_AUTH_TOKEN` | 任意 | controller の `--auth-token` と同じトークン（省略すると登録の PUT に `Authorization` ヘッダーを付けない） |
| `--cors-origin` | `EDAMAME_CORS_ORIGIN` | 任意 | ブラウザからの GET を許可する origin（省略すると CORS を付けず、起動時に警告を出す） |

`--weight`、`--auth-token`、`--cors-origin` 以外の option は必須で、既定値はない。
`--public-url` は `--bind` から推測しない。
`0.0.0.0` など client から到達できないアドレスを待ち受ける場合でも、client から到達できる URL を指定する。
`--controller-url` は query と fragment を含まない絶対 URL とし、末尾の `/` は取り除いて使う。

## 起動と確認

controller を起動する。

```bash
edamame-controller \
  --bind 127.0.0.1:9000 \
  --storage-dir .edamame/controller \
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
  --cache-dir .edamame/worker-a \
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
鍵 URI はセグメントごとに記録されるため、続くセグメントに同じ鍵を使う場合も、そのセグメントの PUT で同じ `key_uri` を指定する。
鍵 URI を変えた PUT を送ると、そのセグメントのプレイリストに新しい鍵 URI が反映される。
鍵 URI を付けない PUT は、そのセグメントだけを暗号化なしに戻す。
client はプレイリストの `#EXT-X-KEY` に書かれた鍵 URI を自分で取得するため、鍵の配信元は client から到達できるようにする。

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

各 quality の公開範囲は、sync を受けた時に、sync が届いている worker が連続してキャッシュしている公開可能範囲のうち最新のものになる。
quality ごとに独立して決まり、quality 間で範囲を一致させる必要はない。
media sequence は前進・後退・飛び得る。
現在の公開範囲を保持している worker がいれば、より新しい候補が出るまで維持する。
現在の公開範囲を保持している worker がいなければ、次の sync で、より古い範囲も含めて配信可能な最新の候補へ更新する。更新までのプレイリストリクエストは 503 になることがある。
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

メディアプレイリストは quality ごとに、公開範囲をキャッシュしている worker から配信先を選ぶ。
候補が複数ある場合は、登録された重みの比に従って選ぶ。
重み 4 の worker と重み 1 の worker があれば、5 回のうち 4 回は重み 4 の worker を選ぶ。

重みは相対値であり、選ばれる回数の比になる。
同じスペックのホストなら同じ値にし、速いホストほど大きい値にする。
例えば 4 vCPU のホストを `--weight 1`、16 vCPU のホストを `--weight 4` にする。
`--weight` を省略した worker は 1 として扱う。

重みを変えたときと、配信先の候補の worker が増減したときは、その時点から重みの比で選び直す。
controller は worker の負荷情報（CPU 使用率、同時接続数、配信量）を扱わない。
そのため、一時的に性能が落ちた worker でも重みの分だけ選ばれる。

controller と worker は同じ版を使う。
worker が古いと `weight` が送られず登録が 400 になり、controller が古いと `weight` が無視される。

## 容量

controller は実行中にセグメントファイルを削除しない。
`storage_dir` の容量は運用者が監視し、必要に応じて古いセグメントファイルを削除する。
削除してもセグメント番号には影響しない。番号は controller のメモリ上のカウンタで決まる。
残したセグメントファイルは、次の実行で同じ番号のセグメントを PUT したときに上書きされる。

worker のキャッシュ上限は `(channel_id, quality)` ごとの上限であり、`cache_dir` 全体の容量上限ではない。
worker は形式に合わないファイルを削除しないため、`cache_dir` の総容量は運用者が監視する。

controller はセグメント識別子とセグメント番号の対応もメモリに保持する。
セグメントファイルよりはずっと小さいが、実行中に PUT したセグメント数に比例して増える。

## 再起動

controller を再起動すると、channel、quality、セグメントのメタデータ、セグメント識別子とセグメント番号の対応、worker の登録は失われる。
セグメント番号は 0 から始まり、新しい識別子に順に割り当てる。
`storage_dir` のセグメントファイルは削除しない。前の実行で保存したファイルは残り、新しいセグメントと番号が重なった時点で上書きされる。
`GET /segments/...` は、同じ番号のファイルがまだ上書きされていなければ前の実行のファイルを返す。
プレイリストは再起動後に PUT されたセグメントファイルから作り直すため、publisher がセグメントファイルを PUT するまで 404 になる。
worker は sync の 404 を契機に再登録し、登録に成功した時点でキャッシュを捨てて取り直す。取得と sync を再開すれば配信先に戻る。
client はプレイリストを取り直し、メディアシーケンスが 0 から始まるプレイリストに合わせて再同期する。

メタデータは最初に成功した PUT の属性から作り直す。
同じ識別子を再 PUT すると新しい番号で保存される。

worker を再起動すると、`cache_dir` を走査してキャッシュしているセグメントファイルの索引を作り直す。
走査では `{channel_id}/{quality}/{sequence}.ts` の形式に合うファイルだけを使う。
形式に合わないファイルは索引に入れず、削除もしない。
索引を作った後、キャッシュ上限を超えたセグメントファイルを削除し、controller に登録する。
登録に成功すると、その時点のキャッシュの索引とファイルを捨てて controller から取り直す。controller が停止中の間だけ、走査した内容を配信する。

worker はセグメントファイルを保存するたびに、`(channel_id, quality)` ごとに、キャッシュ上限までのセグメントファイルを新しい順に残し、古いセグメントファイルを削除する。

起動の順序は問わない。
worker は controller への登録に失敗しても動作を続け、1 秒後に再試行する。
登録に成功するまでは controller から間隔を受け取れないため、1 秒は固定の再試行間隔とする。
登録に成功すると、受け取った間隔で sync を送る。

## 失敗時の扱い

| 状況 | 動作 |
| --- | --- |
| controller が停止した | client はプレイリストを取得できず、worker はキャッシュしているセグメントファイルを配信し続けて sync の失敗を繰り返し、controller の復帰後に再登録してキャッシュを捨てる |
| worker が停止した | controller は `--sync-timeout-ms` の経過後にその worker を配信先の候補から外す。その quality の公開範囲を保持する他の worker があればそれを使い、なければ次の sync で公開範囲が更新されるまで 503 になる |
| セグメントファイルの取得に失敗した | worker はログを出して次の sync で同じセグメントファイルを再取得する。公開範囲を保持していなければ配信先に使われない |
| セグメントファイルが欠落した | controller はその quality のプレイリストを生成せず 422 を返し、キャッシュ済みセグメントファイルの配信は続く |
| セグメントファイルを後から削除した | 番号には影響しない。次の実行で同じ番号のセグメントを PUT したときに上書きされる |
| controller の `storage_dir` への書き込みに失敗した | controller は 500 を返し、書き込み途中のセグメントファイルを読み手に見せず、保存済みとして扱わない |
| worker の `cache_dir` への書き込みに失敗した | worker はログを出し、そのセグメントファイルをキャッシュ済みとして扱わず、次の sync で再取得する |
| worker のトークンが controller と一致しない | controller は登録の PUT に 401 を返し、worker はログを出して 1 秒後に再試行する |
| controller がメタデータを失った | セグメント番号は 0 から始まり、publisher がセグメントファイルを PUT するまでプレイリストは 404 になる。worker は登録時にキャッシュを捨てて取り直す |
| controller の再起動後に同じ識別子を再 PUT した | 新しい番号を割り当てて保存する。前の実行のファイルと同じ番号になった場合は上書きする |
| client が古いプレイリストを持つ | プレイリストに書かれた worker が停止している場合、セグメントファイルの取得は失敗する。controller は `--sync-timeout-ms` の経過後にその worker を候補から外すまで同じ worker を選ぶことがある（重みが複数台にある場合は他の worker も選ばれる） |
| 鍵 URI の配信元が停止した | client は鍵を取得できず、その鍵を使うセグメントを復号できない。controller と worker は鍵を保持しないため、鍵の可用性は publisher の配信元に依存する |
| 公開範囲に鍵 URI の異なるセグメントが混ざった | メディアプレイリストはセグメントごとに `#EXT-X-KEY` を出し分ける。client はセグメントごとに適切な鍵を取得する |
| 外部配信が停止した | controller は外部の状態を確認しないため、外部配信の登録を取り消すか、controller を再起動するまで振り分けを続ける |
| controller が再起動した | 登録された外部配信は失われ、publisher が再登録するまで外部へ振り分けない |

## セキュリティと信頼境界

- controller は `--auth-token` を指定すると PUT に Bearer トークンを要求し、指定しなければ警告を出して PUT を認証しない
- publisher が送るセグメント識別子は文字種を限定し、controller は識別子をファイル名に使わず `storage_dir` の外にファイルを作らない
- worker が取得する URL は `--controller-url` とセグメントの識別子から作り、外部から渡された URL は取得しない
- `--public-url` には client から到達できる URL を指定し、controller はこの URL をプレイリストに書く
- controller は登録された外部配信の URL を取得しない。client が外部配信から直接取得する
- 外部配信へ流すかどうかは接続元の IP で決めるため、プロキシや NAT 配下では複数の client が同じ扱いになる
- CORS は GET の読み取りだけを許可し、`PUT` と `POST` には CORS ヘッダを付けない

## ログ

- ログメッセージは英語とする
- 起動時に待ち受けアドレスを出力する
- sync の失敗とセグメントファイルの取得失敗は標準エラーに出力する
- メトリクス用のエンドポイントは提供しない
