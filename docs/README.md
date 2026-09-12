# Edamame

この文書は Edamame の振る舞い仕様を定める。

Edamame は、ライブ HLS のセグメントファイルを publisher から受け取り、controller が正規データとして保管し、worker が取得してキャッシュする。
client は controller からプレイリストを取得し、プレイリストに書かれた worker からセグメントファイルを取得する。
ここでライブ配信とは、セグメントファイルが継続的に追加され、プレイリストが更新され続ける配信を指す。
VOD は扱わない。

## ドキュメント

- [HTTP API](./api.md)：controller と worker のエンドポイント、ステータスコード、sync の流れ
- [HLS の出力](./hls.md)：マスタープレイリストとメディアプレイリストの形式
- [運用](./operations.md)：CLI、起動、再起動、失敗時の扱い、制約

## 用語

| 用語 | 説明 |
| --- | --- |
| publisher | セグメントファイルを作成し、controller に PUT する外部ソフトウェア |
| client | HLS のプレイリストとセグメントファイルを取得する再生ソフトウェア |
| ライブ配信 | セグメントファイルが継続的に追加され、プレイリストが更新され続ける配信 |
| controller | 正規セグメントファイルの保管、プレイリストの生成、worker の管理と選択を担当する |
| worker | controller からセグメントファイルを取得してキャッシュし、client に配信する |
| channel | `channel_id` で識別する配信の単位 |
| quality | `quality` で識別する channel 内の画質の単位 |
| セグメント | channel と quality に属する映像の断片で、HLS では Media Segment と呼ぶ |
| セグメントファイル | 1 つのセグメントを保存した MPEG-TS ファイル |
| セグメント識別子 | publisher がセグメントごとに付ける名前であり、同じセグメントには quality をまたいで同じ値を使う |
| セグメント番号 | controller が識別子に割り当てるセグメントの連番であり、0 以上の整数 |
| キャッシュ一覧 | worker が現在キャッシュしているセグメントファイルの一覧であり、sync の `cached_segments` で送る |
| 未キャッシュ | worker のキャッシュ一覧に含まれていないセグメントであり、`fetch_segments` で返す |
| キャッシュ対象 | worker が `READY` になるためにキャッシュしなければならないセグメントファイルの集合 |
| 直近のセグメントの範囲 | `--playlist-segments` で決める、最新のセグメント番号から数えた範囲 |
| 配信範囲 | プレイリストに実際に書く、存在する最も古いセグメント番号から最新のセグメント番号までの範囲 |
| キャッシュ上限 | worker が `(channel_id, quality)` ごとにキャッシュするセグメントファイルの最大数であり、`--cache-limit` で指定する |
| マスタープレイリスト | quality ごとのメディアプレイリストを列挙したプレイリスト |
| メディアプレイリスト | 1 つの quality のセグメントを列挙したプレイリスト |

## 責務

**controller** は publisher から PUT されたセグメントファイルを検証し、セグメント識別子にセグメント番号を割り当てて `storage_dir` に正規データとして保存する。
channel と quality ごとにセグメントファイルを管理し、配信範囲を決めてプレイリストを生成する。
worker の登録と同期を受け、`PREPARING`、`READY`、`DEAD` の状態を管理する。
プレイリストを配信する worker を 1 つ選ぶ。

**worker** は controller に登録し、登録応答で受け取った間隔で sync を送る。
sync 応答に従ってセグメントファイルを取得し、`cache_dir` に保存する。
キャッシュしているセグメントファイルを client に配信し、キャッシュ上限を超えたセグメントファイルを削除する。
worker は全体の配置やプレイリストの生成を行わない。

**publisher** はセグメントごとの識別子を付けてセグメントファイルを controller に PUT する。
このとき `duration_ms` と `bandwidth` を指定する。
同じセグメントには quality をまたいで同じ識別子を使う。
publisher はプレイリストを PUT しない。

**client** は controller からプレイリストを取得し、プレイリストに書かれた worker からセグメントファイルを取得する。

## データモデル

セグメントは channel、quality、セグメント番号の組で識別する。
API では `(channel_id, quality, sequence)` の順に表す。
現在の仕様では、1 つのセグメントを 1 つのセグメントファイルとして保存する。
`channel_id`、`quality`、`worker_id` は `[A-Za-z0-9_-]` の 1 文字以上 64 文字以下とする。
`sequence` はセグメント番号を表し、10 進数の u64 として先頭に 0 を付けない。
セグメント番号は、HLS の仕様では media sequence number（メディアシーケンス番号）と呼ばれる。
ファイル名は `{sequence}.ts` だけを許可する。
セグメント識別子は `[A-Za-z0-9_-]` の 1 文字以上 64 文字以下とし、channel 内でセグメントごとに一意とする。
controller は識別子にセグメント番号を 1 対 1 で対応させ、新しい識別子に channel ごとの連番を 0 から割り当てる。
同じ識別子には quality をまたいで同じ番号を割り当てる。
番号と識別子の対応は controller の実行中だけ保持し、起動するたびに 0 から振り直す。

セグメントは次の属性を持つ。

- `duration_ms`：セグメントの長さをミリ秒で表す 500 以上の整数であり、PUT のたびに必須
- `bandwidth`：quality のビットレートを bit/s で表す正の整数であり、PUT のたびに必須
- `resolution`：quality の解像度を `{width}x{height}` で表す任意の属性

channel ごとにセグメント長の基準値を 1 つ持つ。
同じ channel の全セグメントの `duration_ms` は同じ値でなければならない。
基準値は、最初に成功した PUT の `duration_ms` を秒に直し、小数点以下を切り上げた整数（最低 1）とする。
channel が存在する間、この値は変わらない。
メディアプレイリストの `#EXT-X-TARGETDURATION` にはこの基準値を出力する。
HLS のライブ配信ではこの値を途中で変えられないため、controller は最初の PUT で基準値を固定する。
HLS はセグメントごとに異なる長さを許すが、現在の仕様では配信範囲の合計時間が基準値の 3 倍以上になることを単純に保証するため、同じ channel のセグメント長を同じ値に限定する。

`channel_id` と `quality` は、最初のセグメントファイルを PUT したときに作られる。
channel や quality を作る専用の API はない。
`quality` に `master` は使えない。

controller は `{storage_dir}/{channel_id}/{quality}/{sequence}.ts` にセグメントファイルを保存する。
worker は `{cache_dir}/{channel_id}/{quality}/{sequence}.ts` にセグメントファイルを保存する。

## 不変条件

controller が持つセグメントファイルが正規データであり、worker のセグメントファイルは再取得できるコピーである。

同じ channel、quality、セグメント番号のセグメントは 1 つだけ存在する。
同じ識別子で同じバイト列の再 PUT は 200 を返し、同じ識別子でバイト列が違う PUT は 409 を返す。
識別子とセグメント番号の対応は controller のメモリ上だけにあり、controller の再起動で失われる。
controller は起動するたびにセグメント番号を 0 から振り直し、前の実行で保存したファイルと同じ番号になった場合は上書きする。

controller はセグメントファイルを削除しない。
worker だけがキャッシュ上限に従ってセグメントファイルを削除する。

プレイリストはリクエストごとに現在のメタデータから生成する。
controller は以前に生成したプレイリストを保存せず、生成に失敗した場合は古いプレイリストを返さない。

現在の仕様で扱うセグメントファイルは MPEG-TS (`.ts`) だけである。

## プレイリストに含める範囲

quality ごとに、保存されている最大のセグメント番号を最新のセグメント番号として扱う。
プレイリストに含める直近のセグメントの数は `--playlist-segments` で指定する。
`--playlist-segments` は 6 以上とする。
範囲は、最新のセグメント番号から `--playlist-segments` 個前までの範囲とする。
範囲の先頭が負になる場合は 0 からとする。

直近のセグメントの範囲で、存在する最も古いセグメント番号から最新のセグメント番号までに欠落したセグメント番号がある場合、controller はその quality のメディアプレイリストを生成せず、リクエストに 422 を返す。
開始直後などで直近のセグメントの範囲の先頭より古いセグメントがない場合、controller は存在する最も古いセグメント番号から最新のセグメント番号までを配信する。
この制限は、HLS のライブ配信用のプレイリストが、削除後もセグメント長の基準値の 3 倍以上のセグメントを必要とするためである。

同じ識別子を別の quality に PUT すると、先に決まった古いセグメント番号で保存される。
このとき最新のセグメント番号は変わらない。
欠落を埋めた場合は、次のリクエストからプレイリストに反映する。

マスタープレイリストは、channel 内の全 quality の配信範囲が一致する場合だけ生成する。

## worker の状態

| 状態 | 説明 |
| --- | --- |
| `PREPARING` | 登録直後、またはキャッシュ対象のセグメントファイルをキャッシュしていないためプレイリストの選択対象にならない |
| `READY` | キャッシュ対象のセグメントファイルをキャッシュしているためプレイリストの選択対象になる |
| `DEAD` | sync が `sync-timeout-ms` を超えて届いていないためプレイリストの選択対象にならない |

状態は次のように変わる。

- controller はプレイリストリクエストと sync のたびに worker の状態を評価する
- 登録直後の worker は `PREPARING` になる
- 登録を更新した worker は報告済みのキャッシュを無効にして `PREPARING` に戻る
- sync の報告がキャッシュ対象を包含すれば `READY`、包含しなければ `PREPARING` になる
- sync が `sync-timeout-ms` を超えて届かない worker は `DEAD` になる
- `DEAD` の worker が sync を送ると `PREPARING` に戻り、同じ報告でキャッシュ対象を満たせば `READY` になる

キャッシュ対象は、全 channel と全 quality の直近のセグメントの範囲に存在するセグメントをすべて合わせたものである。
channel や quality を追加すると、worker はその範囲のセグメントファイルを取得するまで `PREPARING` になる。

## 配置

controller は `READY` の worker だけをプレイリストに使う。
`READY` の worker が複数ある場合は `worker_id` の昇順で先頭を選ぶ。
`READY` の worker がないプレイリストリクエストには 503 を返し、controller のセグメントファイルの URL には逃がさない。

現在の仕様では負荷分散をしない。
worker の負荷情報は扱わない。

worker は状態に関係なく、キャッシュしているセグメントファイルを配信する。
どの worker をプレイリストに書くかは controller が決める。

## 非目標

- worker の負荷分散
- fMP4、LL-HLS、バイト範囲、暗号化、複数音声
- controller のメタデータの永続化
- メトリクス用のエンドポイント
- CDN 連携
- 認証と TLS
- 複数の controller
