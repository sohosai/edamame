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
| 暗号鍵の URI | publisher が配信する AES-128 の暗号鍵を指す URL であり、セグメント PUT でセグメントごとに指定する |
| キャッシュ一覧 | worker が現在キャッシュしているセグメントファイルの一覧であり、sync の `cached_segments` で送る |
| 未キャッシュ | worker のキャッシュ一覧に含まれていないセグメントであり、`fetch_segments` で返す |
| キャッシュ対象 | 直近のセグメントの範囲のセグメントファイルの集合 |
| 直近のセグメントの範囲 | `--playlist-segments` で決める、最新のセグメント番号から数えた範囲 |
| 公開範囲 | quality ごとに controller がプレイリストに反映すると決めた連続したセグメント番号の範囲 |
| 公開待ち | 未公開セグメントを公開範囲の候補にできるようになるまでの待ち時間 |
| キャッシュ上限 | worker が `(channel_id, quality)` ごとにキャッシュするセグメントファイルの最大数であり、`--cache-limit` で指定する |
| 重み | worker の相対的な配信能力であり、登録時に宣言する 1 以上の整数 |
| マスタープレイリスト | quality ごとのメディアプレイリストを列挙したプレイリスト |
| メディアプレイリスト | 1 つの quality のセグメントを列挙したプレイリスト |

## 責務

**controller** は publisher から PUT されたセグメントファイルを検証し、セグメント識別子にセグメント番号を割り当てて `storage_dir` に正規データとして保存する。
channel と quality ごとにセグメントファイルを管理し、quality ごとに公開範囲を決めてプレイリストを生成する。
worker の登録と sync を受け、sync が届いている worker の中から quality ごとに配信先を選ぶ。

**worker** は controller に登録し、登録応答で受け取った間隔で sync を送る。
sync 応答の `fetch_segments` に従ってセグメントファイルを取得し、`cache_dir` に保存する。
キャッシュしているセグメントファイルを client に配信し、キャッシュ上限を超えたセグメントファイルを削除する。
worker は全体の配置やプレイリストの生成を行わない。

**publisher** はセグメントごとの識別子を付けてセグメントファイルを controller に PUT する。
このとき `duration_ms` と `bandwidth` を指定する。
同じセグメントには quality をまたいで同じ識別子を使う。
セグメントを暗号化する場合は、鍵を指す `key_uri` も指定する。
publisher は暗号鍵を自分で配信し、プレイリストを PUT しない。

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
- `key_uri`：セグメントの復号に使う AES-128 の暗号鍵を指す、`http` または `https` の絶対 URL であり、省略できる任意の属性

channel ごとにセグメント長の基準値を 1 つ持つ。
同じ channel の全セグメントの `duration_ms` は同じ値でなければならない。
基準値は、最初に成功した PUT の `duration_ms` を秒に直し、小数点以下を切り上げた整数（最低 1）とする。
channel が存在する間、この値は変わらない。
メディアプレイリストの `#EXT-X-TARGETDURATION` にはこの基準値を出力する。
HLS のライブ配信ではこの値を途中で変えられないため、controller は最初の PUT で基準値を固定する。
`--playlist-segments` は 6 以上とするが、worker が保持している連続範囲が短い場合は、その短い範囲を公開することがある。

`channel_id` と `quality` は、最初のセグメントファイルを PUT したときに作られる。
channel や quality を作る専用の API はない。
`quality` に `master` は使えない。

controller は `{storage_dir}/{channel_id}/{quality}/{sequence}.ts` にセグメントファイルを保存する。
worker は `{cache_dir}/{channel_id}/{quality}/{sequence}.ts` にセグメントファイルを保存する。

セグメントを暗号化する場合、publisher は `key_uri` に鍵の URL を指定する。
鍵の実体は publisher が配信し、controller と worker は鍵を保存も配信もしない。
`key_uri` を付けないセグメントは暗号化されていないセグメントとして扱う。
controller はセグメント番号ごとに `key_uri` を記録し、値が変わったところをメディアプレイリストの `#EXT-X-KEY` に反映する。
publisher は鍵 URI を何度でも変更でき、controller は変更の履歴を保持して追従する。
公開範囲に鍵 URI の異なるセグメントが混ざる場合は、セグメントごとに `#EXT-X-KEY` を出し分ける。
IV は扱わない。

## 不変条件

controller が持つセグメントファイルが正規データであり、worker のセグメントファイルは再取得できるコピーである。

同じ channel、quality、セグメント番号のセグメントは 1 つだけ存在する。
同じ識別子で同じバイト列の再 PUT は 200 を返し、同じ識別子でバイト列が違う PUT は 409 を返す。
識別子とセグメント番号の対応と鍵 URI の変更履歴は controller のメモリ上だけにあり、controller の再起動で失われる。
controller は起動するたびにセグメント番号を 0 から振り直し、前の実行で保存したファイルと同じ番号になった場合は上書きする。

controller はセグメントファイルを削除しない。
worker だけがキャッシュ上限に従ってセグメントファイルを削除する。

プレイリストはリクエストごとに現在の公開範囲から生成する。

現在の仕様で扱うセグメントファイルは MPEG-TS (`.ts`) だけである。

## プレイリストに含める範囲

quality ごとに、保存されている最大のセグメント番号を最新のセグメント番号として扱う。
プレイリストに含める直近のセグメントの数は `--playlist-segments` で指定する。
`--playlist-segments` は 6 以上とする。
範囲は、最新のセグメント番号から `--playlist-segments` 個前までの範囲とする。
範囲の先頭が負になる場合は 0 からとする。

controller はセグメントファイルを PUT されてもすぐにはプレイリストへ反映しない。
最初の未公開セグメントファイルを PUT した時刻から `--publish-wait-ms` だけ待つ。
追加の PUT では待ち時間を延長しない。
公開待ちの間は、未公開セグメントを公開範囲の候補にもフォールバックにも含めない。
公開待ちが終わった時点で存在するセグメントを公開可能として扱う。

各 quality の公開範囲は、sync が届いている worker が連続してキャッシュしている公開可能範囲のうち、最新のものとする。
公開範囲は quality ごとに独立して決め、quality 間で範囲を一致させる必要はない。
公開範囲は worker の sync を受けた時に更新する。
現在の公開範囲を保持している worker がいなければ、次の sync で、より古い範囲も含めて配信可能な最新の候補へ更新する。更新までのプレイリストリクエストは 503 になることがある。

公開範囲のファイルが欠落している場合、controller はその quality のメディアプレイリストを生成せず、リクエストに 422 を返す。

同じ識別子を別の quality に PUT すると、先に決まった古いセグメント番号で保存される。
このとき最新のセグメント番号は変わらない。


## worker の選択

controller は、sync が `--sync-timeout-ms` 以内に届いている worker だけを配信先の候補にする。
登録を更新した worker は報告済みのキャッシュを無効にする。

メディアプレイリストは quality ごとに配信先を選ぶ。
その quality の公開範囲をキャッシュしている worker が複数ある場合は、登録された重みの比に従って選ぶ。
重みが同じ場合は順番に選ぶ。
重みを変えたときと、配信先の候補が増減したときは、その時点から重みの比に従って選び直す。
quality ごとに別の worker を選んでもよい。
公開範囲または配信先 worker がない quality のメディアプレイリストリクエストには 503 を返す。

## 非目標

- worker の負荷に応じた動的な配置
- fMP4、LL-HLS、バイト範囲、複数音声
- 暗号鍵の保管と配信
- 初期化ベクトル（IV）の指定
- AES-128 以外の暗号化方式
- controller のメタデータの永続化
- メトリクス用のエンドポイント
- CDN 連携
- TLS
- 複数の controller
- 全 quality の公開範囲を一致させること
