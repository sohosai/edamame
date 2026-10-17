# 運用

この文書では、`controller` と `worker` の設定、パラメータ設計とメモリ容量の見積もり、TLS 配信、および再起動や障害時の動作を説明する。
全体構成と用語の定義は [docs/README.md](./README.md) で説明する。

## 構成と接続関係

Edamame は 1 つの `controller` と 1 つ以上の `worker` で構成する。
`publisher` は `controller` にセグメントファイルを `PUT` し、`client` はプレイリストを `controller` から、セグメントファイルを `worker` から取得する。

```text
publisher ── PUT セグメントファイル ────────────────────> controller
worker    ── PUT 登録 / POST sync / GET セグメントファイル ─> controller
client    ── GET プレイリスト (.m3u8) ──────────────────> controller
client    ── GET セグメントファイル (.ts) ──────────────> worker
```

`client` は外部の HLS プレイヤーであり、Edamame の CLI オプションでは設定しない。
`client` には、`controller` のマスタープレイリスト URL（`{controller_url}/channels/{channel_id}/master.m3u8`）を設定する。

## 設定リファレンス

各オプションはコマンドライン引数または環境変数で指定する。
環境変数の名前は、オプション名の `-` を `_` に変えて先頭に `EDAMAME_` を付けたものにする。
両方を指定した場合は、コマンドライン引数を優先する。

### controller の設定

| option | 環境変数 | 必須 | 説明 |
| --- | --- | --- | --- |
| `--bind` | `EDAMAME_BIND` | 必須 | HTTP サーバが待ち受けるアドレス。`client` や `worker` が接続する URL ではない |
| `--storage-capacity` | `EDAMAME_STORAGE_CAPACITY` | 必須 | 正規セグメントファイルをメモリ上に保存する容量（`256MiB` のような 2 進接頭辞付きの値、またはバイト数。1 以上） |
| `--playlist-segments` | `EDAMAME_PLAYLIST_SEGMENTS` | 必須 | メディアプレイリストに含める直近のセグメント数（全 `channel` に共通、6 以上の整数） |
| `--sync-interval-ms` | `EDAMAME_SYNC_INTERVAL_MS` | 必須 | `worker` に指示する `sync` の間隔をミリ秒で指定する（1 以上の整数） |
| `--sync-timeout-ms` | `EDAMAME_SYNC_TIMEOUT_MS` | 必須 | `sync` が届かない `worker` を配信先から外すまでの待ち時間をミリ秒で指定する（1 以上の整数） |
| `--publish-wait-ms` | `EDAMAME_PUBLISH_WAIT_MS` | 任意 | 新しいセグメントを公開する前に `worker` の取得を待つ時間をミリ秒で指定する（0 以上の整数、省略時は `2000` を使い警告を出す） |
| `--auth-token` | `EDAMAME_AUTH_TOKEN` | 任意 | `PUT` の認証に使うトークン（空白と制御文字を除く表示可能 ASCII、省略すると `PUT` を認証せず警告を出す） |
| `--cors-origin` | `EDAMAME_CORS_ORIGIN` | 任意 | ブラウザからの `GET` を許可する origin（`*`、`none`、またはカンマ区切りの origin 一覧。省略すると CORS ヘッダを付けず警告を出す） |
| `--external-min-percent` | `EDAMAME_EXTERNAL_MIN_PERCENT` | 任意 | 外部配信が登録されているときに外部配信へ流す最低の割合（%、0 以上 100 以下、省略時は `1`。公開の遅れが増えると最大 100% まで上昇する） |

### worker の設定

| option | 環境変数 | 必須 | 説明 |
| --- | --- | --- | --- |
| `--worker-id` | `EDAMAME_WORKER_ID` | 必須 | `controller` に登録する `worker` の識別子 |
| `--bind` | `EDAMAME_BIND` | 必須 | HTTP サーバが待ち受けるアドレス |
| `--public-url` | `EDAMAME_PUBLIC_URL` | 必須 | `client` に公開する `worker` の URL。`http` または `https` で始まる絶対 URL（TLS 終端の reverse proxy を置く場合は `https`） |
| `--controller-url` | `EDAMAME_CONTROLLER_URL` | 必須 | `worker` から `controller` に接続するための `http` の絶対 URL |
| `--cache-limit` | `EDAMAME_CACHE_LIMIT` | 必須 | `(channel_id, quality)` ごとのキャッシュ上限（1 以上の整数） |
| `--tls-cert` | `EDAMAME_TLS_CERT` | 任意 | TLS 配信に使う証明書の PEM ファイル。`--tls-key` と組で指定する |
| `--tls-key` | `EDAMAME_TLS_KEY` | 任意 | TLS 配信に使う秘密鍵の PEM ファイル。`--tls-cert` と組で指定する |
| `--acme` | `EDAMAME_ACME` | 任意 | ACME で証明書を取得して TLS 配信する（環境変数で指定する場合は `true` または `false`） |
| `--acme-directory` | `EDAMAME_ACME_DIRECTORY` | 任意 | ACME ディレクトリの `https` URL（省略時は Let's Encrypt の本番環境） |
| `--acme-cache-dir` | `EDAMAME_ACME_CACHE_DIR` | `--acme` 指定時は必須 | ACME のアカウント鍵と証明書の保存先 |
| `--acme-contact` | `EDAMAME_ACME_CONTACT` | 任意 | ACME の連絡先メールアドレス（省略すると起動時に警告を出す） |
| `--weight` | `EDAMAME_WEIGHT` | 任意 | `worker` の重み（1 以上の整数、省略時は `1`） |
| `--auth-token` | `EDAMAME_AUTH_TOKEN` | 任意 | `controller` の `--auth-token` と同じトークン（省略すると登録の `PUT` に `Authorization` ヘッダを付けない） |
| `--cors-origin` | `EDAMAME_CORS_ORIGIN` | 任意 | ブラウザからの `GET` を許可する origin（`*`、`none`、またはカンマ区切りの origin 一覧。省略すると CORS ヘッダを付けず警告を出す） |

`--worker-id`、`--bind`、`--public-url`、`--controller-url`、`--cache-limit` の 5 つは必須であり、既定値はない。
`--public-url` はプレイリスト内のセグメント URL に書き込まれる公開 URL であり、`--bind` の値からは自動生成しない。
`0.0.0.0` など `client` から直接到達できないアドレスで待ち受ける場合でも、`--public-url` には `client` から到達できる URL を指定する。
URL にパスを含めた場合（例：`https://edge.example.test/worker-a`）は、`worker` 側もそのパスを接頭辞とした `{path}/segments/...` でリクエストを待ち受ける。

`--controller-url` には、`worker` から到達できる `http` の絶対 URL だけを指定できる。
この URL には query と fragment を含めず、末尾に `/` がある場合は取り除いて使う。

## パラメータ設計とメモリ見積もり

### sync 間隔と公開待ちの設計

`worker` は登録時に受け取った `sync_interval_ms` の間隔で `controller` に `sync` を送る。
`controller` は、`sync` が `--sync-timeout-ms` の時間を超えて届かない `worker` を配信先の候補から外す。
各パラメータは次の関係を満たすように設定する。

- `--sync-interval-ms`：セグメント長（`duration_ms`）より十分小さい値にする（例：6 秒セグメントに対して `1000` ミリ秒）
- `--sync-timeout-ms`：1 回や 2 回の `sync` が遅れただけで配信先から外れないよう、`--sync-interval-ms` の 3 倍以上を推奨する（例：`10000` ミリ秒）
- `--publish-wait-ms`：1 回目の `sync` でセグメントを取得し、次の `sync` でキャッシュ完了を報告する時間を確保するため、`--sync-interval-ms` の 2 倍を推奨する（例：`2000` ミリ秒）
- `--cache-limit`：`controller` の `--playlist-segments` より 1 以上大きく設定することを推奨する（例：`--playlist-segments 6` に対して `12`）。公開待ちの間も直前の公開範囲と新しい範囲の両方を保持でき、古い範囲へ切り替わりにくくなる

公開範囲が決まる仕組みの詳細は [docs/README.md](./README.md) の「2. worker の同期と公開範囲の決定」で説明する。

### worker の重み付け

`controller` はメディアプレイリストを生成するときに、`quality` ごとに公開範囲をキャッシュしている `worker` から配信先を選ぶ。
候補が複数ある場合は、登録された重み（`--weight`）の比に従って選ぶ。
たとえば重み `4` の `worker` と重み `1` の `worker` があれば、5 回のうち 4 回は重み `4` の `worker` を選ぶ。

重みは相対値であり、選ばれる回数の比になる。
同じ性能のホストなら同じ値にし、帯域や CPU に余裕があるホストほど大きい値にする（例：4 vCPU のホストを `--weight 1`、16 vCPU のホストを `--weight 4` にする）。
`--weight` を省略した `worker` は `1` として扱い、登録時に `0` を送ると `controller` は `400 Bad Request` を返す。

`controller` は `worker` のリアルタイムな負荷情報（CPU 使用率、同時接続数、配信帯域）を収集しないため、一時的に性能が落ちた `worker` でも重みの比に従って選ばれ続ける。

### メモリ容量の見積もり

`controller` はセグメントファイルをメモリ上に保存し、合計バイト数が `--storage-capacity` を超えたときは書き込んだ順に古いセグメントファイルから削除する。
容量はセグメントファイルのバイト数の合計であり、セグメント識別子や暗号化指定などのメタデータは含まない（セグメントファイルを削除したときは、その識別子対応と暗号化指定も合わせて削除される）。

保存容量には、公開範囲と、公開待ちから `worker` が取得し終えるまでに必要な範囲を保持できる大きさを確保する。
容量が足りないと、`controller` が公開範囲のセグメントファイルを削除してしまい、その `quality` のメディアプレイリストは `422 Unprocessable Entity` を返す。
また、セグメントファイル 1 つが `--storage-capacity` より大きい場合、その `PUT` には `413 Payload Too Large` を返して保存しない。

必要な容量の目安は次の式で計算する（1 セットは、同じ時間帯における全 `quality` のセグメントの合計サイズ）。

- `controller` の保存容量の目安：保持したいセグメント数 × 1 セットの最大サイズ × `channel` 数（保持したいセグメント数は、`--playlist-segments` に公開待ちと `sync` 間隔の分を足した数より大きくする）
- `worker` のメモリ使用量の目安：1 セットの最大サイズ × `--cache-limit` × `channel` 数（`--cache-limit` は `(channel_id, quality)` ごとのセグメント数の上限）

たとえば `publisher` の設定例（6 秒セグメント、1080p 12 Mbps、720p 6.5 Mbps、480p 1.4 Mbps）では、3 つの `quality` の合計で 1 セットあたり約 14 MiB になる。
`--playlist-segments 6` に公開待ちと `sync` 間隔の分を足した 8 セグメントを下回らないよう、余裕を見て 10 セグメント分を 3 `channel` で保持すると仮定すると、`controller` の必要量は約 14 MiB × 10 × 3 ≒ 430 MiB となり、`--storage-capacity 512MiB` が目安になる。
同じ構成で `worker` に `--cache-limit 12` を設定した場合、`worker` のメモリ使用量は約 14 MiB × 12 × 3 ≒ 504 MiB が目安になる。

## 通信保護と信頼境界

### TLS 配信と ACME

`worker` は既定では HTTP で待ち受ける。
TLS を終端する reverse proxy を前段に置く場合は、`worker` 側の TLS 設定は不要であり HTTP のまま使える。
`worker` 自身で TLS を終端する場合は、次のどちらかを指定し、`--public-url` には `https` で始まる URL を指定する。

- `--tls-cert` と `--tls-key`：PEM ファイルの証明書と秘密鍵を使う。ファイルの更新時刻を 1 分ごとに確認し、更新されていれば再起動なしで読み直す
- `--acme` と `--acme-cache-dir`：ACME（TLS-ALPN-01 チャレンジ）で `--public-url` のホストに対する証明書を自動取得し、永続ボリュームに保存する。443 番ポートを `worker` に割り当て、80 番ポートは使わない。証明書の更新も `worker` 内部で行うため再起動は不要である（`--acme-directory` で別の ACME ディレクトリを指定できるが、EAB が必要な ZeroSSL には対応していない）

TLS 配信を有効にした `worker` は、公開ルート CA で検証できる証明書を配信している間だけ `controller` への登録と `sync` を行う。
公開ルート CA で信頼されない証明書（ステージング環境の証明書など）では `controller` に登録されない。

### セキュリティと信頼境界

- `controller` は `--auth-token` を指定すると `PUT` に Bearer トークンを要求し、指定しなければ警告を出して `PUT` を認証しない
- `GET` と `POST /workers/{worker_id}/sync` には認証がないため、`controller` の待ち受けポートへは信頼できるネットワークからか、reverse proxy 経由でのみアクセスできるようにする
- `publisher` が送るセグメント識別子は文字種を限定し、`controller` と `worker` は識別子をメモリ上の索引にだけ使う
- `worker` は取得先の URL を `--controller-url` を基底として `channel_id`、`quality`、セグメント番号（`sequence`）から組み立て、外部から渡された任意の URL は取得しない
- `--public-url` には `client` から到達できる URL を指定し、`controller` はこの URL をプレイリストに書く
- `controller` は登録された外部配信の URL を取得せず、`client` が外部配信から直接取得する
- 外部配信への振り分けは、接続元の IP と `channel` から決まる値と、その時点の公開の遅れに応じた外部配信の割合とを比較し、リクエストごとに判定する。外部配信へ流す割合が変われば、同じ接続元と `channel` でも判定結果が変わることがある。reverse proxy や NAT の配下で接続元の IP が同じ `client` は、同じ `channel` と割合であれば同じ判定結果になる
- CORS は `GET` の読み取りだけを許可し、`PUT` と `POST` には CORS ヘッダを付けない

### ログ出力

- ログメッセージはすべて英語で出力する
- 起動時に待ち受けアドレスを標準出力へ出力する
- `sync` の失敗とセグメントファイルの取得失敗は標準エラーへ出力する
- メトリクス用のエンドポイントは提供しない

## 起動と動作確認

`controller` を起動する。

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

`worker` を起動する。

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

セグメントファイルを `PUT` する。レスポンスの `sequence`（この例では `0`）が `controller` の割り当てたセグメント番号である。

```bash
printf 'segment-1' > /tmp/segment-1.ts
curl -i -X PUT \
  -H 'Authorization: Bearer example-token' \
  --data-binary @/tmp/segment-1.ts \
  'http://127.0.0.1:9000/segments/live/720p/segment-1?duration_ms=6000&bandwidth=3000000&resolution=1280x720'
```

セグメントを AES-128 で暗号化する場合は、暗号鍵を指す `key_uri`（`http` または `https` の絶対 URL）を付けて `PUT` する。
暗号鍵の URI はセグメントごとに記録されるため、後続のセグメントに同じ鍵を使う場合も毎回 `key_uri` を指定する。
`client` はプレイリストの `#EXT-X-KEY` に書かれた `key_uri` から直接鍵を取得するため、鍵の配信元は `client` から到達できるようにする。

```bash
curl -i -X PUT \
  -H 'Authorization: Bearer example-token' \
  --data-binary @/tmp/segment-1.ts \
  'http://127.0.0.1:9000/segments/live/720p/segment-1?duration_ms=6000&bandwidth=3000000&resolution=1280x720&key_uri=https%3A%2F%2Fkeys.example.test%2Flive%2Fkey-1'
```

`worker` が次の `sync` でセグメントファイルを取得し、`--publish-wait-ms` が経過するとプレイリストが取得できるようになる。
最初の公開範囲ができるまでは `503 Service Unavailable` を返すことがあるため、`--retry` を付けて確認する。

```bash
curl -i --retry 30 --retry-delay 1 --retry-connrefused \
  http://127.0.0.1:9000/channels/live/720p.m3u8
```

プレイリストに `worker` の URL が書かれていることを確認したうえで、その URL からセグメントファイルを取得する。

```bash
curl -i http://127.0.0.1:9001/segments/live/720p/0.ts
```

## 再起動と障害時の動作

### 再起動時の動作

起動の順序は問わない。
`worker` は `controller` への登録に失敗しても動作を続け、登録に成功するまで 1 秒間隔で再試行する。
登録に成功すると、受け取った `sync_interval_ms` の間隔で `sync` を送る。

`controller` を再起動すると、メモリ上にあるセグメントファイル、`channel`、`quality`、メタデータ、識別子と番号の対応、および `worker` の登録情報はすべて失われる。
再起動後の `controller` ではセグメント番号が `0` から始まり、新しい識別子に順番に割り当てられる。
メタデータも最初に成功した `PUT` の属性から作り直され、以前と同じ識別子が再 `PUT` された場合も新しい番号で保存される。
再起動後に新しいセグメントファイルが `PUT` されるまで、`GET /segments/...` とプレイリストの取得はどちらも `404 Not Found` を返す。

`controller` の再起動後、`worker` は `sync` で `404` を受け取った時点で再登録を行い、登録に成功すると手元のキャッシュをすべて捨てて取り直す。
セグメントの取得と `sync` が進めば、再び配信先の候補に戻る。
`client` はプレイリストを取得し直し、メディアシーケンス番号が `0` から始まる新しいプレイリストに合わせて再生位置を再同期する。

`worker` を再起動すると、メモリ上のキャッシュは失われる。
`worker` は `controller` に再登録し、取得を指示されたセグメントファイルを取り直す。
`controller` が停止している間も、`client` は `worker` がすでにキャッシュしているセグメントファイルを取得できる。

### 失敗時の扱い

| 状況 | 動作 |
| --- | --- |
| controller が停止した | client はプレイリストを取得できない。worker はキャッシュしているセグメントファイルの配信を続けるが、sync は失敗し続ける。controller が復帰すると、worker は再登録してキャッシュを捨てる |
| worker が停止した | controller は `--sync-timeout-ms` の経過後にその worker を配信先の候補から外す。その quality の公開範囲を保持する他の worker があればそれを使い、なければ次の sync で公開範囲が更新されるまで 503 になる |
| セグメントファイルの取得に失敗した | worker はログを出して次の sync で同じセグメントファイルを再取得する。公開範囲を保持していなければ配信先に使われない |
| 公開範囲のセグメントファイルが容量超過で削除された | controller はその quality のプレイリストを生成せず 422 を返し、worker がキャッシュしているセグメントファイルの配信は続く |
| 保存容量より大きいセグメントを PUT した | controller は 413 を返し、セグメントファイルを保存せず、channel の属性もセグメント番号も確定しない |
| ACME の証明書取得に失敗した | worker はログを出して再試行する。取得済みの証明書があればそれを期限まで使い続ける |
| 容量超過で削除された quality に同じ識別子を再 PUT した | 別の quality に識別子が残っていれば同じセグメント番号に保存し直す。古いバイト列をキャッシュしているか、取得中の worker には次の sync で再取得を指示する |
| worker のトークンが controller と一致しない | controller は登録の PUT に 401 を返し、worker はログを出して 1 秒後に再試行する |
| controller がメタデータを失った | セグメント番号は 0 から始まり、publisher がセグメントファイルを PUT するまでプレイリストは 404 になる。worker は登録時にキャッシュを捨てて取り直す |
| controller の再起動後に同じ識別子を再 PUT した | 新しい番号を割り当てて保存する。前の実行のセグメントファイルは残っていない |
| client が古いプレイリストを持つ | プレイリストに書かれた worker が停止している場合、セグメントファイルの取得は失敗する。controller は `--sync-timeout-ms` の経過後にその worker を候補から外すまで同じ worker を選ぶことがある（候補が複数台ある場合は、重みに従って他の worker も選ばれる） |
| 暗号鍵の配信元が停止した | client は鍵を取得できず、その鍵を使うセグメントを復号できない。controller と worker は鍵を保持しないため、鍵を取得できるかどうかは publisher の配信元に依存する |
| 公開範囲に暗号鍵の URI の異なるセグメントが混ざった | メディアプレイリストはセグメントごとに `#EXT-X-KEY` を出し分ける。client はセグメントごとに適切な鍵を取得する |
| 外部配信が停止した | controller は外部の状態を確認しないため、外部配信の登録を取り消すか、controller を再起動するまで振り分けを続ける |
| controller が再起動した | 登録された外部配信は失われ、publisher が再登録するまで外部へ振り分けない |
