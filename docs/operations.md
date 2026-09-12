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

## controller の CLI

| option | 必須 | 説明 |
| --- | --- | --- |
| `--bind` | 必須 | HTTP server が待ち受けるアドレスであり、client や worker が接続する URL ではない |
| `--storage-dir` | 必須 | 正規セグメントファイルを保存するディレクトリ |
| `--playlist-segments` | 必須 | メディアプレイリストに含める直近のセグメント数（全 channel に共通、6 以上の整数） |
| `--sync-interval-ms` | 必須 | worker に指示する sync の間隔をミリ秒で指定する（1 以上の整数） |
| `--sync-timeout-ms` | 必須 | worker を `DEAD` と見なすまでの sync の待ち時間をミリ秒で指定する（1 以上の整数） |
| `--cors-origin` | 任意 | ブラウザからの GET を許可する origin（省略すると CORS を付けず、起動時に警告を出す） |

## worker の CLI

| option | 必須 | 説明 |
| --- | --- | --- |
| `--worker-id` | 必須 | controller に登録する worker の識別子 |
| `--bind` | 必須 | HTTP server が待ち受けるアドレス |
| `--public-url` | 必須 | client に公開する worker の URL であり、`http://` で始まる絶対 URL |
| `--controller-url` | 必須 | controller に接続するための、worker から到達できる URL |
| `--cache-dir` | 必須 | セグメントファイルのキャッシュ先ディレクトリ |
| `--cache-limit` | 必須 | `(channel_id, quality)` ごとのキャッシュ上限（1 以上の整数） |
| `--cors-origin` | 任意 | ブラウザからの GET を許可する origin（省略すると CORS を付けず、起動時に警告を出す） |

`--cors-origin` 以外の option は必須で、既定値はない。
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
  --cors-origin '*'
```

セグメントファイルを PUT する。

```bash
printf 'segment-1' > /tmp/segment-1.ts
curl -i -X PUT \
  --data-binary @/tmp/segment-1.ts \
  'http://127.0.0.1:9000/segments/live/720p/segment-1?duration_ms=6000&bandwidth=3000000&resolution=1280x720'
```

レスポンスの `sequence` が controller の割り当てたセグメント番号である。
この例では 0 になる。

worker は次の sync でセグメントファイルを取得する。
メディアプレイリストは worker が `READY` になるまで 503 を返すことがある。
503 の間は再試行する。

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
controller は `--sync-timeout-ms` を超えて sync が届かない worker を `DEAD` にして、プレイリストの選択対象から外す。

`--sync-timeout-ms` は `--sync-interval-ms` の 3 倍以上を推奨する。
1 回や 2 回の sync が遅れても `DEAD` にしないためである。

新しいセグメントファイルを PUT した直後は、worker が取得して次の sync で報告するまでプレイリストが 503 になることがある。
この間を短くするには `--sync-interval-ms` をセグメントの `duration_ms` より十分小さくする。

worker のキャッシュ上限（`--cache-limit`）は controller の `--playlist-segments` 以上にする。
キャッシュ上限が `--playlist-segments` より小さい場合、セグメントファイルが増えると worker はキャッシュ対象のセグメントファイルを削除して `READY` を失い、`PREPARING` と `READY` を繰り返す。

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
worker は sync の 404 を契機に再登録し、登録に成功した時点でキャッシュを捨てて取り直す。新しい範囲のセグメントファイルを取得すれば `READY` に戻る。
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
| worker が停止した | controller は `--sync-timeout-ms` の経過後に `DEAD` にし、他の `READY` の worker があればプレイリストはその worker を使い、なければ 503 になる |
| セグメントファイルの取得に失敗した | worker はログを出して次の sync で同じセグメントファイルを再取得し、`PREPARING` のままになる |
| セグメントファイルが欠落した | controller はその quality のプレイリストを生成せず 422 を返し、キャッシュ済みセグメントファイルの配信は続く |
| セグメントファイルを後から削除した | 番号には影響しない。次の実行で同じ番号のセグメントを PUT したときに上書きされる |
| controller の `storage_dir` への書き込みに失敗した | controller は 500 を返し、書き込み途中のセグメントファイルを読み手に見せず、保存済みとして扱わない |
| worker の `cache_dir` への書き込みに失敗した | worker はログを出し、そのセグメントファイルをキャッシュ済みとして扱わず、次の sync で再取得する |
| controller がメタデータを失った | セグメント番号は 0 から始まり、publisher がセグメントファイルを PUT するまでプレイリストは 404 になる。worker は登録時にキャッシュを捨てて取り直す |
| controller の再起動後に同じ識別子を再 PUT した | 新しい番号を割り当てて保存する。前の実行のファイルと同じ番号になった場合は上書きする |
| client が古いプレイリストを持つ | プレイリストに書かれた worker が停止している場合、セグメントファイルの取得は失敗し、client がプレイリストを取得し直すと controller は別の `READY` の worker を選ぶ |

## セキュリティと信頼境界

- private network で使い、認証と TLS はない
- publisher が送るセグメント識別子は文字種を限定し、controller は識別子をファイル名に使わず `storage_dir` の外にファイルを作らない
- worker が取得する URL は `--controller-url` とセグメントの識別子から作り、外部から渡された URL は取得しない
- `--public-url` には client から到達できる URL を指定し、controller はこの URL をプレイリストに書く
- CORS は GET の読み取りだけを許可し、`PUT` と `POST` には CORS ヘッダを付けない

## ログ

- ログメッセージは英語とする
- 起動時に待ち受けアドレスを出力する
- sync の失敗とセグメントファイルの取得失敗は標準エラーに出力する
- メトリクス用のエンドポイントは提供しない
