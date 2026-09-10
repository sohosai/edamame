# Edamame

この文書は Edamame の振る舞い仕様を定める。

Edamame は、live HLS の segment を publisher から受け取り、controller が正規データとして保管し、cache が取得して保持する。
client は controller から playlist を取得し、playlist に書かれた cache から segment を取得する。

## ドキュメント

- [HTTP API](./api.md)：controller と cache のエンドポイント、ステータスコード、sync の流れ。
- [HLS の出力](./hls.md)：master playlist と media playlist の形式。
- [運用](./operations.md)：CLI、起動、再起動、失敗時の扱い、制約。

## 用語

| 用語 | 説明 |
| --- | --- |
| publisher | segment を作成し、controller に PUT する外部ソフトウェア。 |
| client | HLS の playlist と segment を取得する再生ソフトウェア。 |
| controller | 正規 segment の保管、playlist の生成、cache の管理と選択を担当する。 |
| cache | controller から segment を取得して保持し、client に配信する。 |
| channel | 配信の単位。`channel_id` で識別する。 |
| quality | channel 内の画質の単位。`quality` で識別する。 |
| segment | channel と quality に属する MPEG-TS のデータ。 |
| sequence | segment の順序を表す 0 以上の整数。 |
| cached_segments | cache が現在保持している segment の一覧。 |
| live window | 再生対象にする直近の segment の範囲。 |
| keep-segments | cache が保持する segment の最大数。 |
| master playlist | quality ごとの media playlist を列挙した playlist。 |
| media playlist | 1 つの quality の segment を列挙した playlist。 |

## 責務

**controller** は publisher から PUT された segment を検証し、`storage_dir` に正規データとして保存する。
channel と quality ごとに segment を管理し、live window と playlist を生成する。
cache の登録と同期を受け、`PREPARING`、`READY`、`DEAD` の状態を管理する。
playlist を配信する cache を 1 つ選ぶ。

**cache** は controller の sync 応答に従って segment を取得し、`cache_dir` に保存する。
保持している segment を client に配信し、`keep-segments` を超えた segment を削除する。
cache は全体の配置や playlist の生成を行わない。

**publisher** は segment を `{sequence}.ts` の形式で controller に PUT する。
このとき `duration_ms` と `bandwidth` を指定する。
publisher は playlist を PUT しない。

**client** は controller から playlist を取得し、playlist に書かれた cache から segment を取得する。

## データモデル

segment は `(channel_id, quality, sequence)` の組で識別する。
`channel_id`、`quality`、`cache_id` は `[A-Za-z0-9_-]` の 1 文字以上 64 文字以下とする。
`sequence` は 10 進数の u64 とし、先頭に 0 を付けない。
ファイル名は `{sequence}.ts` だけを許可する。

segment は次の属性を持つ。

- `duration_ms`：segment の長さ。500 以上の整数でミリ秒を表す。PUT のたびに必須である。
- `bandwidth`：quality のビットレート。正の整数で bit/s を表す。PUT のたびに必須である。
- `resolution`：quality の解像度。任意で、`{width}x{height}` の形式にする。

channel ごとに target duration を 1 つ持つ。
同じ channel の全 segment の `duration_ms` は同じ値でなければならない。
target duration は、channel の最初に成功した PUT の `duration_ms` を秒に直し、小数点以下を切り上げた整数（最低 1）とする。
channel が存在する間、target duration は変わらない。

`channel_id` と `quality` は、最初の segment を PUT したときに作られる。
channel や quality を作る専用の API はない。
`quality` に `master` は使えない。

controller は `{storage_dir}/{channel_id}/{quality}/{sequence}.ts` に segment を保存する。
cache は `{cache_dir}/{channel_id}/{quality}/{sequence}.ts` に segment を保存する。

## 不変条件

controller が持つ segment が正規データであり、cache の segment は再取得できるコピーである。

同じ `(channel_id, quality, sequence)` の segment は 1 つだけ存在する。
バイト列と属性が同じ再 PUT は 200 を返し、バイト列または属性が違う PUT は 409 を返す。

controller は segment を削除しない。
cache だけが `keep-segments` に従って segment を削除する。

playlist はリクエストごとに現在のメタデータから生成する。
controller は以前に生成した playlist を保存せず、生成に失敗した場合は古い playlist を返さない。

v1 で扱う segment は MPEG-TS (`.ts`) だけである。

## live window

quality ごとに、保存されている最大の sequence を live edge と呼ぶ。
live window の長さは `--live-window` で指定する。
`--live-window` は 6 以上とする。
`--live-window` の値を `live_window` とすると、範囲は `live_edge - live_window + 1` から `live_edge` までとする。
`live_edge - live_window + 1` が負になる場合は 0 からとする。

live edge までの間に欠落した sequence がある場合、その quality の media playlist は生成できない。
開始直後などで window の先頭より古い segment がない場合は、存在する最も古い sequence から live edge までを配信する。
この制限は、HLS の live playlist が削除後も 3 target duration 以上の segment を必要とするためである。

live edge より古い sequence の segment を後から PUT しても live edge は変わらない。
欠落を埋めた場合は、次のリクエストから playlist に反映する。

master playlist は、channel 内の全 quality の配信範囲が一致する場合だけ生成する。

## cache の状態

| 状態 | 説明 |
| --- | --- |
| `PREPARING` | 登録直後、または保持している segment が必要集合を満たしていない。playlist の選択対象にならない。 |
| `READY` | 保持している segment が必要集合を満たしている。playlist の選択対象になる。 |
| `DEAD` | sync が `sync-timeout-ms` を超えて届いていない。playlist の選択対象にならない。 |

状態は次のように変わる。

- controller は playlist リクエストと sync のたびに cache の状態を評価する。
- 登録直後の cache は `PREPARING` になる。
- sync の報告が必要集合を包含すれば `READY`、包含しなければ `PREPARING` になる。
- sync が `sync-timeout-ms` を超えて届かない cache は `DEAD` になる。
- `DEAD` の cache が sync を送ると `PREPARING` に戻る。同じ報告で必要集合を満たせば `READY` になる。

必要集合は、全 channel と全 quality の live window に存在する segment の和集合である。
channel や quality を追加すると、cache はその live window を取得するまで `PREPARING` になる。

## 配置

controller は `READY` の cache だけを playlist に使う。
`READY` の cache が複数ある場合は `cache_id` の昇順で先頭を選ぶ。
`READY` の cache がない playlist リクエストには 503 を返し、controller の segment URL には逃がさない。

v1 は負荷分散をしない。
cache の負荷情報は扱わない。

cache は状態に関係なく、保持している segment を配信する。
どの cache を playlist に書くかは controller が決める。

## v1 の非目標

- cache の負荷分散
- fMP4、LL-HLS、byte-range、暗号化、複数音声
- controller のメタデータの永続化
- メトリクス用のエンドポイント
- CDN 連携
- 認証と TLS
- 複数の controller
