# 運用

## 構成

Edamame は 1 つの controller と 1 つ以上の cache で構成する。
publisher は controller に segment を PUT する。
client は controller から playlist を取得し、cache から segment を取得する。

```
publisher -- PUT segment --> controller
client ---- GET playlist --> controller
client ---- GET segment ---> cache
controller <-- sync/GET ---- cache
```

client は外部の HLS player であり、Edamame の CLI option では設定しない。
client には、controller に到達できる URL を設定する。

## controller の CLI

| option | 必須 | 説明 |
| --- | --- | --- |
| `--bind` | 必須 | HTTP server が待ち受けるアドレス。client や cache が接続する URL ではない。 |
| `--storage-dir` | 必須 | 正規 segment を保存するディレクトリ。 |
| `--live-window` | 必須 | media playlist に含める直近の segment 数。全 channel に共通で、6 以上の整数。 |
| `--sync-timeout-ms` | 必須 | cache を `DEAD` と見なすまでの sync の待ち時間。ミリ秒。 |

## cache の CLI

| option | 必須 | 説明 |
| --- | --- | --- |
| `--cache-id` | 必須 | controller に登録する cache の識別子。 |
| `--bind` | 必須 | HTTP server が待ち受けるアドレス。 |
| `--public-url` | 必須 | client に公開する cache の URL。`http://` で始まる絶対 URL。 |
| `--controller-url` | 必須 | controller に接続するための、cache から到達できる URL。 |
| `--cache-dir` | 必須 | 保持する segment を保存するディレクトリ。 |
| `--keep-segments` | 必須 | `(channel_id, quality)` ごとに保持する segment の最大数。 |
| `--sync-interval-ms` | 必須 | controller に sync を送る間隔。ミリ秒。 |

すべての option は必須で、既定値はない。
controller は起動時に `--live-window` が 6 以上でなければ起動しない。
`--public-url` は `--bind` から推測しない。
`0.0.0.0` など client から到達できないアドレスを待ち受ける場合でも、client から到達できる URL を指定する。
`--controller-url` は query と fragment を含まない絶対 URL とし、末尾の `/` は取り除いて使う。

## 起動と確認

controller を起動する。

```bash
edamame-controller \
  --bind 127.0.0.1:9000 \
  --storage-dir .edamame/controller \
  --live-window 6 \
  --sync-timeout-ms 10000
```

cache を起動する。

```bash
edamame-cache \
  --cache-id cache-a \
  --bind 127.0.0.1:9001 \
  --public-url http://127.0.0.1:9001 \
  --controller-url http://127.0.0.1:9000 \
  --cache-dir .edamame/cache-a \
  --keep-segments 12 \
  --sync-interval-ms 1000
```

segment を PUT する。

```bash
printf 'segment-101' > /tmp/101.ts
curl -i -X PUT \
  --data-binary @/tmp/101.ts \
  'http://127.0.0.1:9000/segments/live/720p/101.ts?duration_ms=6000&bandwidth=3000000&resolution=1280x720'
```

cache は次の sync で segment を取得する。
media playlist は cache が `READY` になるまで 503 を返すことがある。
503 の間は再試行する。

```bash
curl -i --retry 30 --retry-delay 1 --retry-connrefused \
  http://127.0.0.1:9000/channels/live/720p.m3u8
```

playlist に cache の URL が書かれていることを確認し、segment を取得する。

```bash
curl -i http://127.0.0.1:9001/segments/live/720p/101.ts
```

## sync とタイムアウト

cache は `--sync-interval-ms` ごとに controller に sync を送る。
controller は `--sync-timeout-ms` を超えて sync が届かない cache を `DEAD` にして、playlist の選択対象から外す。

`--sync-timeout-ms` は `--sync-interval-ms` の 3 倍以上を推奨する。
1 回や 2 回の sync が遅れても `DEAD` にしないためである。

新しい segment を PUT した直後は、cache が取得して次の sync で報告するまで playlist が 503 になることがある。
この間を短くするには `--sync-interval-ms` を segment の `duration_ms` より十分小さくする。

cache の `--keep-segments` は controller の `--live-window` 以上にする。
`--keep-segments` が `--live-window` より小さい場合、live window が育つと cache は必要な segment を削除して `READY` を失い、`PREPARING` と `READY` を繰り返す。

## 容量

controller は segment を削除しない。
`storage_dir` の容量は運用者が監視し、必要に応じて古い segment を削除する。
削除した segment が再計算後の配信範囲に欠落として残る場合、その quality の playlist は 422 になる。

cache の `--keep-segments` は `(channel_id, quality)` ごとの上限であり、`cache_dir` 全体の容量上限ではない。
cache は形式に合わないファイルを削除しないため、`cache_dir` の総容量は運用者が監視する。

## 再起動

controller を再起動すると、channel、quality、segment のメタデータと cache の登録は失われる。
`storage_dir` のファイルは残るため、`GET /segments/...` は再起動前からある segment を返す。
playlist を生成するには publisher が segment を再 PUT してメタデータを作り直す。
cache は sync の 404 を契機に再登録し、保持している segment を報告する。再 PUT が済めば `READY` に戻る。

再起動後も `storage_dir` のファイルが正規データである。
同じ識別子のファイルが存在する場合、PUT のバイト列がファイルと一致すれば 200、違えば 409 とする。
ファイルが存在しない場合は 201 で保存する。
メタデータは最初に成功した PUT の属性から作り直す。

cache を再起動すると、`cache_dir` を走査して保持している segment の索引を作り直す。
走査では `{channel_id}/{quality}/{sequence}.ts` の形式に合うファイルだけを使う。
形式に合わないファイルは索引に入れず、削除もしない。
索引を作った後、`keep-segments` を超えた segment を削除し、controller に登録する。

cache は segment を保存するたびに、`(channel_id, quality)` ごとに最新の `--keep-segments` 個を残し、古い segment を削除する。

起動の順序は問わない。
cache は controller への登録に失敗しても動作を続け、次の `--sync-interval-ms` で再試行する。

## 失敗時の扱い

| 状況 | 動作 |
| --- | --- |
| controller が停止した | client は playlist を取得できない。cache は保持している segment を配信し続け、sync の失敗を繰り返す。controller が復帰すると cache は再登録する。 |
| cache が停止した | controller は `--sync-timeout-ms` の経過後に `DEAD` にする。他の `READY` の cache があれば playlist はその cache を使う。なければ 503 になる。 |
| segment の取得に失敗した | cache はログを出し、次の sync で同じ segment を再取得する。cache は `PREPARING` のままになる。 |
| segment が欠落した | その quality の playlist は 422 になる。保持済み segment の配信は続く。 |
| storage への書き込みに失敗した | controller は 500 を返す。segment は書き込みが完了してから見える。 |
| controller がメタデータを失った | publisher が再 PUT するまで playlist は 404 になる。cache は保持している segment を報告できる。 |
| client が古い playlist を持つ | playlist に書かれた cache が停止している場合、segment の取得は失敗する。client が playlist を取得し直すと、controller は別の `READY` の cache を選ぶ。 |

## セキュリティと信頼境界

- private network で使う。認証と TLS はない。
- publisher が送る識別子は文字種を限定する。controller は `storage_dir` の外にファイルを作らない。
- cache が取得する URL は `--controller-url` と segment の識別子から作る。外部から渡された URL は取得しない。
- `--public-url` には client から到達できる URL を指定する。controller はこの URL を playlist に書く。

## ログ

- ログメッセージは英語とする。
- 起動時に待ち受けアドレスを出力する。
- sync の失敗と segment の取得失敗は標準エラーに出力する。
- メトリクス用のエンドポイントは v1 では提供しない。
