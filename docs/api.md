# HTTP API

この文書は controller と cache が提供する HTTP API の契約を定める。
外部ソフトウェアは publisher と client としてこの API を使う。

## 共通ルール

- 例として controller の URL を `http://127.0.0.1:9000`、cache の `public_url` を `http://127.0.0.1:9001` とする。
- controller の `--bind` は待ち受けるアドレスであり、client や cache が接続する URL ではない。`0.0.0.0` などで待ち受ける場合、client は controller に到達できる URL を base URL として使い、cache は `--controller-url` に到達できる URL を指定する。
- `channel_id`、`quality`、`cache_id` は `[A-Za-z0-9_-]` の 1 文字以上 64 文字以下とする。それ以外は 400 で拒否する。
- `quality` に `master` は使えない。使った場合は 400 で拒否する。
- `sequence` は 10 進数の u64 とし、先頭に 0 を付けない。ファイル名は `{sequence}.ts` だけを許可する。
- JSON を送るリクエストの Content-Type は `application/json` とする。
- エラー本文は英語の plain text とし、Content-Type は `text/plain; charset=utf-8` とする。
- playlist の Content-Type は `application/vnd.apple.mpegurl`、segment の Content-Type は `application/octet-stream` とする。
- 認証はない。private network で使う前提とする。
- ステータスコードは 400、404、422、503 の順に判定する。入力が不正なら 400、対象が存在しなければ 404、playlist を生成できなければ 422、配信できる cache がなければ 503 を返す。
- 予期しない内部エラーは 500 を返す。

## controller の API

### segment を登録する

```
PUT /segments/{channel_id}/{quality}/{filename}?duration_ms={duration_ms}&bandwidth={bandwidth}&resolution={width}x{height}
```

publisher が正規 segment を登録する。
`duration_ms` と `bandwidth` は必須で、`resolution` は任意とする。
`duration_ms` は 500 以上の整数、`bandwidth` は正の整数、`resolution` は `{width}x{height}` の形式にする。

例：

```bash
curl -i -X PUT \
  --data-binary @101.ts \
  'http://127.0.0.1:9000/segments/live/720p/101.ts?duration_ms=6000&bandwidth=3000000&resolution=1280x720'
```

ステータス：

- `201 Created`：新しい segment を保存した。
- `200 OK`：同じ識別子、同じバイト列、同じ属性の segment がすでにある。
- `400 Bad Request`：識別子、ファイル名、`duration_ms`、`bandwidth`、`resolution` の形式が不正である。または `duration_ms` が 500 未満である。
- `409 Conflict`：同じ segment に異なるバイト列または属性がある。同じ quality に異なる `bandwidth` や `resolution` がある。または同じ channel の他の segment と違う `duration_ms` を指定した。
- `500 Internal Server Error`：保存に失敗した。

同じ segment の再 PUT では、`duration_ms` が確立済みの値と違う場合も 409 とする。
`resolution` は省略できる。確立済みの値と違う値を指定した場合は 409 とする。
`bandwidth` は quality ごとに 1 つの値に固定する。2 つ目以降の segment で違う値を指定した場合は 409 とする。

同じ channel の全 segment の `duration_ms` は同じ値でなければならない。
最初に成功した PUT の `duration_ms` から target duration を決める。
target duration は `duration_ms` を秒に直し、小数点以下を切り上げた整数（最低 1）とする。
2 つ目以降の PUT で違う `duration_ms` を指定した場合は 409 とする。

controller はメタデータがない場合でも、同じ識別子のファイルが存在すればバイト列を比較する。
バイト列が一致すれば 200、違えば 409 とする。

### segment を取得する

```
GET /segments/{channel_id}/{quality}/{filename}
```

cache が正規 segment を取得するときに使う。
controller は `storage_dir` のファイルを返す。
メタデータがない segment でも、ファイルが存在すれば 200 を返す。

ステータス：

- `200 OK`：segment のバイト列を返す。
- `400 Bad Request`：識別子またはファイル名が不正である。
- `404 Not Found`：segment が存在しない。

### master playlist を取得する

```
GET /channels/{channel_id}/master.m3u8
```

出力は [HLS の出力](./hls.md) に従う。

ステータス：

- `200 OK`：master playlist を返す。
- `400 Bad Request`：`channel_id` が不正である。
- `404 Not Found`：channel が存在しない。
- `422 Unprocessable Entity`：quality の配信範囲が一致しない。または quality の media playlist を生成できない。
- `503 Service Unavailable`：`READY` の cache がない。

### media playlist を取得する

```
GET /channels/{channel_id}/{quality}.m3u8
```

出力は [HLS の出力](./hls.md) に従う。

ステータス：

- `200 OK`：media playlist を返す。
- `400 Bad Request`：`channel_id` または `quality` が不正である。
- `404 Not Found`：channel または quality が存在しない。
- `422 Unprocessable Entity`：live edge までの間に欠落した sequence がある。
- `503 Service Unavailable`：`READY` の cache がない。

### cache を登録する

```
PUT /caches/{cache_id}
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
パスを含めてもよい。その場合はパスの後ろに `/segments/...` を付けた URL を playlist に書く。
cache は起動時に登録する。
controller が cache を知らない場合は、sync の 404 を契機に再登録する。

ステータス：

- `201 Created`：新しい cache を `PREPARING` で登録し、タイムアウトの基準時刻を現在時刻にする。
- `200 OK`：登録済みの cache の `public_url` を更新した。状態もタイムアウトの基準時刻も変えない。基準時刻は sync だけで更新する。
- `400 Bad Request`：`cache_id` または `public_url` が不正である。

`DEAD` の cache が登録を更新しても状態は `DEAD` のままとし、次の sync で `PREPARING` または `READY` にする。

### cache を同期する

```
POST /caches/{cache_id}/sync
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
    { "channel_id": "live", "quality": "720p", "filename": "102.ts" }
  ]
}
```

cache は `--sync-interval-ms` ごとにこのエンドポイントを呼ぶ。
リクエストの `cached_segments` には、その時点で保持している segment をすべて入れる。
controller は cache の状態を評価し、`state` と `fetch_segments` を返す。
`fetch_segments` は必要集合のうち `cached_segments` に含まれない segment である。
`state` は `PREPARING` または `READY` になる。
`DEAD` は controller がタイムアウトで付ける状態であり、sync を受けた cache は `PREPARING` に戻ってから評価される。

現在の必要集合に含まれない segment を `cached_segments` に入れてもよい。
その segment は `READY` の判定には使わない。

ステータス：

- `200 OK`：sync を受け付けた。
- `400 Bad Request`：`cached_segments` の識別子が不正である。
- `404 Not Found`：cache が登録されていない。cache は再登録する。

## cache の API

### segment を配信する

```
GET /segments/{channel_id}/{quality}/{filename}
```

cache が保持している segment を client に返す。
cache は保持していない segment を controller から取得しない。
取得は sync の `fetch_segments` を契機に行う。

ステータス：

- `200 OK`：segment のバイト列を返す。
- `400 Bad Request`：識別子またはファイル名が不正である。
- `404 Not Found`：cache が segment を保持していない。

## sync の流れ

sync は cache が開始する pull 方式である。
controller から cache へ segment のバイト列を PUT しない。

```
cache                                controller
  |                                       |
  |-- POST /caches/{id}/sync ------------>|
  |   cached_segments                     |
  |                                       |  状態を評価する
  |<-- state, fetch_segments -------------|
  |                                       |
  |-- GET /segments/... ----------------->|
  |<-- bytes -----------------------------|
  |                                       |
  |   次の sync で保持状況を報告する      |
```

手順は次のとおり。

1. cache は `--sync-interval-ms` ごとに sync を送る。
2. controller は cache の状態を評価し、`fetch_segments` を返す。
3. cache は `fetch_segments` の各項目を `{controller-url}/segments/{channel_id}/{quality}/{filename}` から GET する。
4. cache は取得した segment を保存し、次の sync で `cached_segments` に含めて報告する。
