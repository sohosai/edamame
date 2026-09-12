# HTTP API

この文書は controller と worker が提供する HTTP API の仕様を定める。
外部ソフトウェアは publisher と client としてこの API を使う。

## 共通ルール

- 例として controller の URL を `http://127.0.0.1:9000`、worker の `public_url` を `http://127.0.0.1:9001` とする
- controller の `--bind` は待ち受けるアドレスであり、client や worker が接続する URL ではないため、`0.0.0.0` などで待ち受ける場合、client は controller に到達できる URL を base URL として使い、worker は `--controller-url` に到達できる URL を指定する
- `channel_id`、`quality`、`worker_id` は `[A-Za-z0-9_-]` の 1 文字以上 64 文字以下とし、それ以外は 400 で拒否する
- `quality` に `master` は使えず、使った場合は 400 で拒否する
- セグメント識別子（`identifier`）は `[A-Za-z0-9_-]` の 1 文字以上 64 文字以下とし、それ以外は 400 で拒否する
- `sequence` はセグメント番号を表し、10 進数の u64 として先頭に 0 を付けず、ファイル名は `{sequence}.ts` だけを許可する
- JSON を送るリクエストの Content-Type は `application/json` とする
- JSON を返すレスポンスの Content-Type は `application/json` とする
- エラー本文は英語の plain text とし、Content-Type は `text/plain; charset=utf-8` とする
- プレイリストの Content-Type は `application/vnd.apple.mpegurl`、セグメントファイルの Content-Type は `application/octet-stream` とする
- 認証はなく、private network で使う前提とする
- ブラウザからは GET だけ cross-origin で読める。許可する origin は controller と worker の `--cors-origin` で指定し、省略すると CORS ヘッダを返さない
- ステータスコードは 400、404、422、503 の順に判定し、入力が不正なら 400、対象が存在しなければ 404、プレイリストを生成できなければ 422、配信できる worker がなければ 503 を返す
- 予期しない内部エラーは 500 を返す

## controller の API

### セグメントファイルを登録する

```
PUT /segments/{channel_id}/{quality}/{identifier}?duration_ms={duration_ms}&bandwidth={bandwidth}&resolution={width}x{height}
```

publisher が正規セグメントファイルを登録する。
`identifier` はセグメントごとに publisher が付ける識別子であり、同じセグメントには quality をまたいで同じ値を使う。
`duration_ms` と `bandwidth` は必須で、`resolution` は任意とする。
`duration_ms` は 500 以上の整数、`bandwidth` は正の整数、`resolution` は `{width}x{height}` の形式にする。

例：

```bash
curl -i -X PUT \
  --data-binary @segment-1.ts \
  'http://127.0.0.1:9000/segments/live/720p/segment-1?duration_ms=6000&bandwidth=3000000&resolution=1280x720'
```

レスポンス：

```json
{
  "sequence": 0
}
```

controller は起動するたびに channel ごとの連番を 0 から新しい識別子に割り当てる。
同じ channel では、同じ識別子には quality が違っても同じ番号を割り当てる。
同じ識別子の再 PUT には、割り当て済みの番号を返す。
新しい識別子に割り当てた番号のセグメントファイルがすでにある場合（前の起動のファイルが残っている場合など）は、そのファイルを上書きする。

ステータス：

- `201 Created`：新しいセグメントファイルを保存した
- `200 OK`：同じ識別子、同じ quality、同じバイト列のセグメントファイルがすでにある
- `400 Bad Request`：識別子、`duration_ms`、`bandwidth`、`resolution` の形式が不正、または `duration_ms` が 500 未満
- `409 Conflict`：同じ識別子で違うバイト列のセグメントファイルがある、同じ quality に異なる `bandwidth` や `resolution` がある、または同じ channel の他のセグメントファイルと違う `duration_ms` を指定した
- `500 Internal Server Error`：セグメント番号の上限に達した、またはディスク容量不足、権限不足、読み取り専用ファイルシステム、I/O エラーなどで保存に失敗した

同じセグメントファイルの再 PUT では、`duration_ms` が確立済みの値と違う場合も 409 とする。
`resolution` は省略できる。確立済みの値と違う値を指定した場合は 409 とする。
`bandwidth` は quality ごとに 1 つの値に固定する。2 つ目以降のセグメントファイルで違う値を指定した場合は 409 とする。

同じ channel の全セグメントの `duration_ms` は同じ値でなければならない。
最初に成功した PUT の `duration_ms` からセグメント長の基準値を決める。
基準値は `duration_ms` を秒に直し、小数点以下を切り上げた整数（最低 1）とする。
2 つ目以降の PUT で違う `duration_ms` を指定した場合は 409 とする。

controller を再起動すると識別子とセグメント番号の対応は失われ、セグメント番号は 0 から始まる。
再起動後に同じ識別子を PUT すると、新しい番号を割り当てて保存する。
前の起動で保存したセグメントファイルは削除せず、新しいセグメントと番号が重なった時点で上書きする。

### セグメントファイルを取得する

```
GET /segments/{channel_id}/{quality}/{filename}
```

worker が正規セグメントファイルを取得するときに使う。
controller は `storage_dir` のファイルを返す。
メタデータがないセグメントファイルでも、ファイルが存在すれば 200 を返す。

ステータス：

- `200 OK`：セグメントファイルのバイト列を返す
- `400 Bad Request`：識別子またはファイル名が不正である
- `404 Not Found`：セグメントファイルが存在しない

### マスタープレイリストを取得する

```
GET /channels/{channel_id}/master.m3u8
```

出力は [HLS の出力](./hls.md) に従う。

ステータス：

- `200 OK`：マスタープレイリストを返す
- `400 Bad Request`：`channel_id` が不正である
- `404 Not Found`：channel が存在しない
- `422 Unprocessable Entity`：quality の配信範囲が一致しない、または quality のメディアプレイリストを生成できない
- `503 Service Unavailable`：`READY` の worker がない

### メディアプレイリストを取得する

```
GET /channels/{channel_id}/{quality}.m3u8
```

出力は [HLS の出力](./hls.md) に従う。

ステータス：

- `200 OK`：メディアプレイリストを返す
- `400 Bad Request`：`channel_id` または `quality` が不正である
- `404 Not Found`：channel または quality が存在しない
- `422 Unprocessable Entity`：直近のセグメントの範囲で最新のセグメント番号までの間に欠落したセグメント番号があり、controller がメディアプレイリストを生成できない
- `503 Service Unavailable`：`READY` の worker がない

### worker を登録する

```
PUT /workers/{worker_id}
```

リクエスト：

```json
{
  "public_url": "http://127.0.0.1:9001"
}
```

`public_url` は `http://` で始まる絶対 URL とする。
末尾の `/` は取り除いて保存する。
query と fragment を含めない。
パスを含めてもよい。その場合はパスの後ろに `/segments/...` を付けた URL をプレイリストに書く。
worker は起動時に登録する。
controller が worker を知らない場合は、sync の 404 を契機に再登録する。

レスポンス：

```json
{
  "sync_interval_ms": 1000
}
```

worker は `sync_interval_ms` の間隔で sync を送る。
`sync_interval_ms` は正の整数とし、worker は次に登録するまでこの値を使う。
ステータスが 201 と 200 のどちらでも、レスポンスは同じ形式とする。

ステータス：

- `201 Created`：新しい worker を `PREPARING` で登録し、タイムアウトの基準時刻を現在時刻にする
- `200 OK`：登録済みの worker の `public_url` を更新し、報告済みのキャッシュを無効にして `PREPARING` にする。タイムアウトの基準時刻は変えず、sync だけで更新する
- `400 Bad Request`：`worker_id` または `public_url` が不正である

worker は登録のたびにキャッシュを捨てて取り直すため、controller も登録に合わせて報告済みのキャッシュを無効にする。
`DEAD` の worker が登録を更新すると `PREPARING` になり、次の sync で `PREPARING` または `READY` になる。

### worker を同期する

```
POST /workers/{worker_id}/sync
```

リクエスト：

```json
{
  "cached_segments": [
    { "channel_id": "live", "quality": "720p", "sequence": 101 }
  ]
}
```

レスポンス：

```json
{
  "state": "PREPARING",
  "fetch_segments": [
    { "channel_id": "live", "quality": "720p", "sequence": 102 }
  ]
}
```

worker は登録時に受け取った `sync_interval_ms` の間隔でこのエンドポイントを呼ぶ。
リクエストのキャッシュ一覧（`cached_segments`）には、その時点でキャッシュしているセグメントファイルをすべて入れる。
controller は worker の状態を評価し、`state` と `fetch_segments` を返す。
`fetch_segments` には、キャッシュ対象のうちキャッシュ一覧に含まれないセグメント（未キャッシュのセグメント）を入れる。
ここでキャッシュ対象とは、全 channel と全 quality の直近のセグメントの範囲に存在するセグメントの集合である。
`fetch_segments` の各項目は `channel_id`、`quality`、`sequence` を持ち、worker は `{sequence}.ts` を取得する。
`state` は `PREPARING` または `READY` になる。
`DEAD` は controller がタイムアウトで付ける状態であり、sync を受けた worker は `PREPARING` に戻ってから評価される。

現在のキャッシュ対象に含まれないセグメントをキャッシュ一覧に入れてもよい。
そのセグメントは `READY` の判定には使わない。

ステータス：

- `200 OK`：sync を受け付けた
- `400 Bad Request`：キャッシュ一覧（`cached_segments`）の識別子が不正である
- `404 Not Found`：worker が登録されていないため再登録する

## worker の API

### セグメントファイルを配信する

```
GET /segments/{channel_id}/{quality}/{filename}
```

worker がキャッシュしているセグメントファイルを client に返す。
worker はキャッシュしていないセグメントファイルを controller から取得しない。
取得は sync の `fetch_segments` に従って行う。

ステータス：

- `200 OK`：セグメントファイルのバイト列を返す
- `400 Bad Request`：識別子またはファイル名が不正である
- `404 Not Found`：worker がセグメントファイルをキャッシュしていない

## sync の流れ

sync は worker が開始する pull 方式である。
controller から worker へセグメントファイルのバイト列を PUT しない。

```
worker                           controller
  |                                       |
  |-- POST /workers/{id}/sync ----------->|
  |   cached_segments                     |
  |                                       |  状態を評価する
  |<-- state, fetch_segments -------------|
  |                                       |
  |-- GET /segments/... ----------------->|
  |<-- bytes -----------------------------|
  |                                       |
  |   次の sync でキャッシュ内容を報告する |
```

手順は次のとおり。

1. worker は登録時に受け取った `sync_interval_ms` の間隔で sync を送る。
2. controller は worker の状態を評価し、`fetch_segments` を返す。
3. worker は `fetch_segments` の各項目を `{controller-url}/segments/{channel_id}/{quality}/{sequence}.ts` から GET する。
4. worker は取得したセグメントファイルを保存し、次の sync でキャッシュ一覧に含めて報告する。
