# HTTP API

この文書は controller と worker が提供する HTTP API の仕様を定める。
外部ソフトウェアは publisher と client としてこの API を使う。

## 共通ルール

- 例として controller の URL を `http://127.0.0.1:9000`、worker の `public_url` を `http://127.0.0.1:9001` とする
- controller の `--bind` は待ち受けるアドレスであり、client や worker が接続する URL ではない。`0.0.0.0` などで待ち受ける場合、client は controller に到達できる URL を base URL として使い、worker は同様に到達できる URL を `--controller-url` に指定する
- `channel_id`、`quality`、`worker_id` は `[A-Za-z0-9_-]` の 1 文字以上 64 文字以下とし、それ以外は 400 で拒否する
- `quality` に `master` は使えず、使った場合は 400 で拒否する
- セグメント識別子（`identifier`）は `[A-Za-z0-9_-]` の 1 文字以上 64 文字以下とし、それ以外は 400 で拒否する
- `sequence` はセグメント番号を表し、10 進数の u64 として先頭に 0 を付けず、ファイル名は `{sequence}.ts` だけを許可する
- JSON を送るリクエストの Content-Type は `application/json` とする
- JSON を返すレスポンスの Content-Type は `application/json` とする
- エラー本文は英語の plain text とし、Content-Type は `text/plain; charset=utf-8` とする
- プレイリストの Content-Type は `application/vnd.apple.mpegurl`、セグメントファイルの Content-Type は `application/octet-stream` とする
- controller は `--auth-token` を指定したときだけ PUT に `Authorization: Bearer {token}` を要求し、ヘッダーがない、またはトークンが一致しない場合は 401 を返す。GET と POST は認証を要求しない
- ブラウザからは GET だけ cross-origin で読める。許可する origin は controller と worker の `--cors-origin` で指定し、省略すると CORS ヘッダを返さない
- ステータスコードは 401、400、404、422、503 の順に判定し、認証がなければ 401、入力が不正なら 400、対象が存在しなければ 404、公開範囲のセグメントファイルが欠落していれば 422、公開範囲または配信先 worker がなければ 503 を返す
- 予期しない内部エラーは 500 を返す

## 死活監視

controller と worker は、死活監視用の `GET /health` を提供する。

| プロセス | パス | 応答 |
| --- | --- | --- |
| controller | `/health` | `200 ok` |
| worker | `/health` | `200 ok` |
| worker（`path_prefix` あり） | `{path_prefix}/health` | `200 ok` |

認証は不要で、プロセスが HTTP リクエストを受け付けられることだけを返す。依存先の状態は確認しない。

## controller の API

### セグメントファイルを登録する

```
PUT /segments/{channel_id}/{quality}/{identifier}?duration_ms={duration_ms}&bandwidth={bandwidth}&resolution={width}x{height}&key_uri={key_uri}&iv={iv}
```

publisher が正規セグメントファイルを登録する。
`identifier` はセグメントごとに publisher が付ける識別子であり、同じセグメントには quality をまたいで同じ値を使う。
`duration_ms` と `bandwidth` は必須で、`resolution` と `key_uri` と `iv` は任意とする。
`duration_ms` は 500 以上の整数、`bandwidth` は正の整数、`resolution` は `{width}x{height}` の形式にする。
IV は query の `iv` で指定する。
`iv` は 16 byte を表す 32 文字の hex とし、大文字小文字は問わない。
controller は小文字にそろえて保存する。
`iv` は `key_uri` と組で使い、`key_uri` を指定しない PUT に `iv` だけを付けても受け付けない。
`key_uri` と `iv` は他の属性と異なり、変更しても競合として扱わない。
同じ識別子の再 PUT では、最後に受け取った組でそのセグメントの指定を置き換える。
省略した場合は暗号化なしに戻す。
controller が `--auth-token` を指定している場合は `Authorization: Bearer {token}` を付ける。

例：

```bash
curl -i -X PUT \
  -H 'Authorization: Bearer example-token' \
  --data-binary @segment-1.ts \
  'http://127.0.0.1:9000/segments/live/720p/segment-1?duration_ms=6000&bandwidth=3000000&resolution=1280x720'
```

暗号化するセグメントは暗号鍵の URI と IV を付けて PUT する。

```bash
curl -i -X PUT \
  -H 'Authorization: Bearer example-token' \
  --data-binary @segment-1.ts \
  'http://127.0.0.1:9000/segments/live/720p/segment-1?duration_ms=6000&bandwidth=3000000&resolution=1280x720&key_uri=https%3A%2F%2Fkeys.example.test%2Flive%2Fkey-1&iv=9f2c4a1d6b8e0f3a5c7d1e2b4a6c8d0e'
```

レスポンス：

```json
{
  "sequence": 0
}
```

controller は新しい識別子に channel ごとの連番を割り当てる。
連番は起動するたびに 0 から始める。
同じ channel では、同じ識別子には quality が違っても同じ番号を割り当てる。
同じ識別子の再 PUT には、割り当て済みの番号を返す。

ステータス：

- `201 Created`：新しいセグメントファイルを保存した
- `200 OK`：同じ識別子、同じ quality、同じバイト列のセグメントファイルがすでにある
- `401 Unauthorized`：`Authorization` ヘッダーがない、またはトークンが一致しない
- `400 Bad Request`：識別子、`duration_ms`、`bandwidth`、`resolution`、`key_uri`、`iv` の形式が不正、`iv` を `key_uri` なしで指定した、または `duration_ms` が 500 未満
- `409 Conflict`：保存されている同じ識別子で違うバイト列のセグメントファイルがある、同じ quality に異なる `bandwidth` や `resolution` がある、または同じ channel の他のセグメントファイルと違う `duration_ms` を指定した
- `413 Payload Too Large`：セグメントファイルが `--storage-capacity` より大きい
- `500 Internal Server Error`：セグメント番号の上限に達した

同じセグメントファイルの再 PUT では、`duration_ms` がすでに決まっている値と違う場合も 409 とする。
`resolution` は省略できる。
すでに決まっている値と違う値を指定した場合は 409 とする。
`bandwidth` は quality ごとに 1 つの値に固定する。
2 つ目以降のセグメントファイルで違う値を指定した場合は 409 とする。

同じ channel の全セグメントの `duration_ms` は同じ値でなければならない。
最初に成功した PUT の `duration_ms` からセグメント長の基準値を決める。
基準値は `duration_ms` を秒に直し、小数点以下を切り上げた整数（最低 1）とする。
2 つ目以降の PUT で違う `duration_ms` を指定した場合は 409 とする。

controller を再起動すると保存済みのセグメントファイルと識別子とセグメント番号の対応は失われ、セグメント番号は 0 から始まる。
再起動後に同じ識別子を PUT すると、新しい番号を割り当てて保存する。
容量超過で削除された識別子も、別の quality に同じ識別子が保存されていなければ、再 PUT には新しい番号を割り当てる。

### channel の外部配信を登録する

```
PUT /channels/{channel_id}/external
```

publisher が channel の外部配信の master m3u8 の URL を登録する。
`external_m3u8` は host を持つ `http` または `https` の絶対 URL とする。
fragment は含めず、query は含めてもよい。
`null` を送ると登録を取り消す。
controller は登録された URL を取得せず、client が外部配信から直接取得する。
channel は最初のセグメントファイルを PUT したときに作られる。
channel がまだない場合は 404 を返す。

リクエスト：

```json
{ "external_m3u8": "https://px.example.test/abc/def.m3u8" }
```

```json
{ "external_m3u8": null }
```

例：

```bash
curl -i -X PUT \
  -H 'Authorization: Bearer example-token' \
  -H 'Content-Type: application/json' \
  -d '{"external_m3u8":"https://px.example.test/abc/def.m3u8"}' \
  'http://127.0.0.1:9000/channels/live/external'
```

ステータス：

- `204 No Content`：登録した
- `400 Bad Request`：`channel_id` または `external_m3u8` の形式が不正である
- `401 Unauthorized`：`--auth-token` を指定しているのにトークンがない、または一致しない
- `404 Not Found`：channel が存在しない

### セグメントファイルを取得する

```
GET /segments/{channel_id}/{quality}/{filename}
```

worker が正規セグメントファイルを取得するときに使う。
controller はメモリに保存しているセグメントファイルを返す。
容量を超えて削除されたセグメントファイルと、controller の再起動で失われたセグメントファイルは 404 になる。

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
- `302 Found`：外部配信へ振り分ける。`Location` に登録された外部配信の master m3u8 の URL を返す
- `400 Bad Request`：`channel_id` が不正である、または query の `external` に 1 と 0 以外の値を指定した
- `404 Not Found`：channel が存在しない
- `422 Unprocessable Entity`：公開範囲のセグメントファイルが欠落している
- `503 Service Unavailable`：配信できる quality がない

quality 間で公開範囲が違ってもよい。
query に `external=1` を付けると外部配信へ振り分け、`external=0` を付けると自前のマスタープレイリストを返す。
省略時は、接続元の IP と channel から決まる値と、その時点の公開の遅れに応じた外部配信の割合とを比較し、リクエストごとに振り分けを判定する。
割合が変われば、同じ接続元と channel でも判定結果が変わることがある。
`external=0` は publisher が自前の採番を確認するために使う。
外部配信が登録されていなければ 302 は返さず、`external=1` でも自前のマスタープレイリストを返す。

### メディアプレイリストを取得する

```
GET /channels/{channel_id}/{quality}.m3u8
```

出力は [HLS の出力](./hls.md) に従う。

ステータス：

- `200 OK`：メディアプレイリストを返す
- `400 Bad Request`：`channel_id` または `quality` が不正である
- `404 Not Found`：channel または quality が存在しない
- `422 Unprocessable Entity`：公開範囲のセグメントファイルが欠落している
- `503 Service Unavailable`：その quality の公開範囲または配信先 worker がない

### worker を登録する

```
PUT /workers/{worker_id}
```

リクエスト：

```json
{
  "public_url": "http://127.0.0.1:9001",
  "weight": 1
}
```

`public_url` は `http` または `https` で始まる絶対 URL とする。
末尾の `/` は取り除いて保存する。
query と fragment を含めない。
パスを含めてもよい。
その場合はパスの後ろに `/segments/...` を付けた URL をプレイリストに書く。
`weight` は worker の重みを表す 1 以上の整数とする。
controller は配信先の worker を選ぶときにこの値を使い、重みが大きい worker ほど多く選ばれる。
worker は起動時に登録する。
controller が `--auth-token` を指定している場合は `Authorization: Bearer {token}` を付ける。
worker は `--auth-token` を指定すると、この PUT に同じトークンを付ける。
controller に worker が登録されていない場合、worker は sync の 404 を受けて再登録する。

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

- `201 Created`：新しい worker を登録し、タイムアウトの基準時刻を現在時刻にする
- `200 OK`：登録済みの worker の `public_url` と `weight` を更新し、報告済みのキャッシュを無効にする。タイムアウトの基準時刻は変えず、sync だけで更新する
- `401 Unauthorized`：`Authorization` ヘッダーがない、またはトークンが一致しない
- `400 Bad Request`：`worker_id`、`public_url` または `weight` が不正である

worker は登録のたびにキャッシュを捨てて取り直すため、controller も登録に合わせて報告済みのキャッシュを無効にする。
sync が `--sync-timeout-ms` を超えて届かない worker は配信先の候補から外れる。

### worker を同期する

```
POST /workers/{worker_id}/sync
```

リクエスト：

```json
{
  "cached_segments": [
    { "channel_id": "live", "quality": "720p", "sequence": 101 }
  ],
  "fetching_segments": [
    { "channel_id": "live", "quality": "720p", "sequence": 102 }
  ]
}
```

レスポンス：

```json
{
  "fetch_segments": [
    { "channel_id": "live", "quality": "720p", "sequence": 102 }
  ]
}
```

worker は登録時に受け取った `sync_interval_ms` の間隔でこのエンドポイントを呼ぶ。
リクエストのキャッシュ一覧（`cached_segments`）には、その時点でキャッシュしているセグメントファイルをすべて入れる。
取得中の一覧（`fetching_segments`）には、その時点で取得しているセグメントを入れる。
controller は同じセグメント番号に別のバイト列で保存し直したとき、この一覧とキャッシュ一覧を使って、古い内容を保持する worker に再取得を指示する。
controller は quality ごとの公開範囲を評価し、`fetch_segments` を返す。
`fetch_segments` には、キャッシュ対象のうちキャッシュ一覧に含まれないセグメント（未キャッシュのセグメント）を入れる。
同じセグメント番号に別のバイト列で保存し直したセグメントも `fetch_segments` に入る。worker は取得し直してキャッシュを上書きする。
ここでキャッシュ対象とは、全 channel と全 quality の直近のセグメントの範囲に存在するセグメントの集合である。
`fetch_segments` の各項目は `channel_id`、`quality`、`sequence` を持ち、worker は `{sequence}.ts` を取得する。
controller は最初の未公開セグメントファイルを PUT した時刻から、`--publish-wait-ms` で指定した時間を公開待ちとする。
追加の PUT では公開待ちを延長しない。
公開待ちの間は、未公開セグメントを公開範囲の候補に含めない。
公開待ちが終わった範囲について、sync が届いている worker が連続してキャッシュしている最新範囲を、その quality の公開範囲にする。
公開範囲は quality ごとに独立して決める。

controller は sync が `--sync-timeout-ms` 以内に届いている worker だけを配信先の候補にする。
応答に worker の状態は含めない。

ステータス：

- `200 OK`：sync を受け付けた
- `400 Bad Request`：`worker_id`、キャッシュ一覧（`cached_segments`）、取得中の一覧（`fetching_segments`）の識別子が不正である
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
  |                                       |  公開範囲を評価する
  |<-- fetch_segments --------------------|
  |                                       |
  |-- GET /segments/... ----------------->|
  |<-- bytes -----------------------------|
  |                                       |
  |   次の sync でキャッシュ内容を報告する |
```

手順は次のとおり。

1. worker は登録時に受け取った `sync_interval_ms` の間隔で sync を送る。
2. controller は quality ごとの公開範囲を評価し、`fetch_segments` を返す。
3. worker は `fetch_segments` の各項目を `{controller-url}/segments/{channel_id}/{quality}/{sequence}.ts` から GET する。
4. worker は取得したセグメントファイルを保存し、次の sync でキャッシュ一覧に含めて報告する。
5. controller は公開待ちが終わっていれば quality ごとに公開範囲を進め、次のプレイリストリクエストから新しい範囲を返す。
