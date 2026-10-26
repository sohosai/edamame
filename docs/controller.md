# controller

controller は、publisher が送るセグメントの原本をメモリに保存し、採番、worker の管理、プレイリストの生成を担当する。
セグメントの暗号化と鍵の配信は行わない。
client が取得するセグメントの URL には worker の公開 URL を使い、worker 不在時に controller 自身を代替の配信先にはしない。

## 必要な環境

- publisher と worker から controller の待ち受けアドレスへ接続できること
- client からプレイリストの URL へ接続できること
- `--public-url` が各 peer、worker、client から接続でき、このインスタンス固有の URL であること
- セグメントの保持に必要なメモリを確保すること（[容量の見積もり](./operations.md#パラメータ設計とメモリ見積もり)）

controller は HTTP で待ち受ける。
`--public-url` を HTTPS にしても TLS は有効にならないため、HTTPS で公開する場合は TLS 終端を別途用意する。
認証のない GET と sync もあるため、公開する範囲を決めてから起動する（[通信の保護](./operations.md#通信の保護と公開範囲)）。

## 最小起動

controller と worker をまとめて起動する場合は、リポジトリのルートで `docker compose up -d` を実行する（[compose.yaml](../compose.yaml)）。
以下では controller だけをソースから起動する。
リポジトリのルートで起動する。
この例は同一ホスト上の検証用であり、外部の client には `127.0.0.1` の代わりに到達可能な名前を使う。

```bash
cargo run --locked -p edamame-controller -- \
  --bind 127.0.0.1:9000 \
  --public-url http://127.0.0.1:9000 \
  --storage-capacity 512MiB \
  --playlist-segments 6 \
  --sync-interval-ms 1000 \
  --sync-timeout-ms 10000 \
  --publish-wait-ms 2000 \
  --auth-token example-token \
  --cors-origin '*'
```

別のターミナルで応答を確認する。

```bash
curl --fail --show-error http://127.0.0.1:9000/health
curl --fail --show-error http://127.0.0.1:9000/internal/cluster/health
```

`/health` はステータス `200` と本文 `ok` を返す。
役割の確認には `/internal/cluster/health` の `role` と `leader_url` を使う。
起動直後は leader が未確定で、配信用の API が一時的に `503` を返すことがある。
worker と publisher を起動するまでは、応答できても映像を配信できる状態ではない。

## 設定

CLI と環境変数の両方に指定した場合は CLI を優先する。
必須項目に既定値はない。

| オプション | 環境変数 | 必須条件 | 説明 |
| --- | --- | --- | --- |
| `--bind` | `EDAMAME_BIND` | 必須 | HTTP の待ち受けアドレス。接続先として公開する URL とは別に指定する |
| `--public-url` | `EDAMAME_PUBLIC_URL` | 必須 | インスタンス固有の HTTP / HTTPS 絶対 URL。query と fragment は不可。パス接頭辞は使える |
| `--storage-capacity` | `EDAMAME_STORAGE_CAPACITY` | 必須 | セグメント本体の合計容量。バイト数または `KiB` / `MiB` / `GiB` / `TiB` 付きで指定し、1 以上とする |
| `--playlist-segments` | `EDAMAME_PLAYLIST_SEGMENTS` | 必須 | 全 channel 共通の公開セグメント数の上限。6 以上の整数 |
| `--sync-interval-ms` | `EDAMAME_SYNC_INTERVAL_MS` | 必須 | worker に伝える同期間隔。ミリ秒、1 以上 |
| `--sync-timeout-ms` | `EDAMAME_SYNC_TIMEOUT_MS` | 必須 | worker の最終同期から配信先候補として扱う時間。ミリ秒、1 以上 |
| `--publish-wait-ms` | `EDAMAME_PUBLISH_WAIT_MS` | 任意 | 新規セグメントの公開待ち。ミリ秒、0 以上。省略時は `2000` を使い警告を出す |
| `--auth-token` | `EDAMAME_AUTH_TOKEN` | 必須 | PUT と heartbeat に使う共通 Bearer token。空白と制御文字を除く表示可能 ASCII。全 peer、worker、publisher に同じ値を指定する |
| `--cors-origin` | `EDAMAME_CORS_ORIGIN` | 任意 | GET の読み取りを許可する origin。`*`、`none`、またはカンマ区切りの一覧。`*` と `none` は他の値と混在不可。省略時はヘッダーを付けず警告を出す |
| `--external-min-percent` | `EDAMAME_EXTERNAL_MIN_PERCENT` | 任意 | 登録された外部配信へ流す最低割合。0 以上 100 以下の整数、既定値 `1` |
| `--metric` | `EDAMAME_METRIC` | 任意 | leader の優先度。0 以上 4294967295 以下、既定値 `100`。大きい値を優先する |
| `--seed` | `EDAMAME_SEED` | 任意 | 最初に接触する controller の HTTP / HTTPS URL。カンマ区切りと繰り返しを許可し、重複と自分の URL は除く。省略時は自分 1 台で始める |

`--public-url` にパスを含めた場合は、health を含む API をその接頭辞の下で公開する。
URL は `--bind` から自動生成しない。
メトリクスの送信設定は [可観測性](./observability.md) を参照する。

## 保存と採番

セグメントの採番と属性の固定規則は次のとおりである。

- 最初の PUT で channel と quality を作り、channel ごとにセグメント番号を `0` から割り当てる
- 同じ時間帯の映像は全 quality で同じ `identifier` を使い、同じ番号にそろえる。
  異なる時間帯の映像に同じ識別子を使い回さない
- 同じ quality に保存済みの識別子で同じバイト列を再 PUT すると既存の番号を返し、異なるバイト列は拒否する（暗号化指定の更新や属性の固定条件を含む再 PUT の扱いは [HTTP API](./api.md#セグメント属性の一貫性と再登録の扱い) を参照する）
- 同じ channel の `duration_ms` と、同じ quality の `bandwidth` は固定する。
  `resolution` は未設定なら後から指定できるが、一度決まった値は変更できない
- channel の最初の `duration_ms` を秒に直して切り上げた値が、HLS の `#EXT-X-TARGETDURATION` になる

メモリ上の保存容量と削除の規則は次のとおりである。

- 保存容量は全 channel / quality のセグメント本体の合計バイト数に適用する。
  メタデータ、受信中の body、送信中の参照などを含むプロセス全体のメモリ上限ではない
- 1 ファイルが保存容量または HTTP リクエスト上限を超える場合は保存しない
- 容量を超過すると、セグメント番号ではなく書き込み順に古いファイルから削除し、対応する暗号化指定と識別子の索引も取り除く
- 識別子と番号の対応は、その識別子を持つファイルが別の quality に残る間は維持する。
  別の quality にも残っていなければ、同じ識別子の再 PUT に新しい番号を割り当てる
- 別の quality に識別子が残っている状態で、削除された quality へ再 PUT すると同じ番号に保存し直す。
  古いバイト列をキャッシュしているか取得中の worker には、次の sync で再取得を指示する

## 公開範囲の決定

PUT の成功は、すぐにプレイリストへ掲載されることを意味しない。
未取得のセグメントを掲載すると client の取得が失敗するため、controller は公開待ちと worker のキャッシュ報告を使って公開範囲を決める。

### キャッシュ対象

各 quality の最新番号から `--playlist-segments` 個分さかのぼった範囲を、すべての worker のキャッシュ対象とする。
先頭が負になる場合は `0` から数える。
worker ごとに別のセグメントを配置する方式ではない。

### 公開待ち

最初の未公開セグメントの保存後、`--publish-wait-ms` の待ち時間を開始する。
待ち時間中に後続の PUT が来ても終了時刻を延ばさず、その時点の新規セグメントをまとめて公開可能にする。
待ち時間中のセグメントは、古い公開範囲へ戻す候補にも含めない。
同じ quality に保存済みのデータを再 PUT しても、新しい公開待ちは開始しない。

### 連続範囲の選択

公開範囲は worker の sync を受けたときに更新し、プレイリストの GET では更新しない。
quality ごとに次の規則で選ぶ。

- 最終 sync が `--sync-timeout-ms` 以内の worker を候補にする
- 1 台の worker が公開可能なセグメントを途切れずに持つ連続範囲を選び、最大 `--playlist-segments` 個を載せる
- 現在の範囲を配信できる worker がいれば、より新しい候補ができるまでその範囲を維持する
- 現在の範囲を配信できる worker がいなくなれば、より古い範囲も候補にして最新の連続範囲へ切り替える

連続範囲が短い場合は設定数より少なくなり、quality 間で公開範囲が異なることもある。
切り替えによってメディアシーケンス番号は戻ったり飛んだりするため、連続再生は保証しない。
公開範囲がなく、または配信できる worker がいない場合は `503`、公開範囲の原本が容量超過などで欠落した場合は `422` になる。
出力タグとステータスの判定順は [HLS の出力](./hls.md) を参照する。

## 配信先 worker の選択

メディアプレイリストのリクエストごとに、次の規則で配信先の worker を選ぶ。

- その quality の公開範囲をすべてキャッシュしている稼働中の worker から 1 台選び、応答内のセグメント URL はすべて同じ worker を使う
- 候補が複数あれば `weight` の比で選び、同じ重みなら順番に選ぶ。
  重みや候補の構成が変わった時点で選択の周期をリセットする
- 重みはリクエストを振り分ける比率であり、リアルタイムの CPU 使用率、接続数、帯域使用量に合わせた自動調整ではない
- worker は再登録すると、報告済みのキャッシュを無効にして取り直す

## 外部配信への振り分け

channel に外部のマスタープレイリスト URL を 1 つ登録すると、master のリクエストを `302` で外部へ振り分けられる。
外部のセグメントは取得も保存もしない。
外部 URL の登録、解除、`external=0/1` の指定方法は [HTTP API](./api.md#マスタープレイリストを取得するclient-用) を参照する。

振り分け割合は、配信可能な quality の最新番号と公開末尾の遅れから求め、その中で最大の割合を使う（公開待ち相当の数は `--publish-wait-ms / duration_ms` を切り上げて求める）。

- 遅れが `--playlist-segments` 個に達したら `100%` にする
- それ未満で、遅れが公開待ち相当のセグメント数以内なら `--external-min-percent` にする
- その間では最低割合から `100%` まで線形に増やす
- 配信可能な quality がなければ `100%` にする

判定と運用上の注意点は次のとおりである。

- 接続元の TCP の IP と channel から決まる値を割合と比較するため、リクエストごとの独立した乱数選択ではない。
  同じ IP と channel なら、割合が同じ間は同じ結果になる
- `Forwarded` と `X-Forwarded-For` は参照しないため、NAT やリバースプロキシの配下では同じ接続元 IP の client が同じ判定になる
- 外部 URL に接続できるかや配信状態は確認しない。
  外部配信を止めるときは登録を解除する

## 再起動と失敗時の扱い

セグメント、採番、channel / quality の属性、暗号化指定、worker の登録、外部配信の登録はメモリにだけ保持する。
再起動または leader 交代が起きたときのそれぞれの動作は次のとおりである。

- controller：保持していた状態をすべて失い、採番も `0` から始まる
- worker：sync の `404` で再登録して手元のキャッシュを捨て、新しいセグメントを取り直す
- publisher：外部配信を登録していた場合は、利用者が再登録する必要がある
- client：番号のリセットに追従できないプレイヤーは、再生を開始し直す

publisher の停止と controller の再起動は、影響が異なる。
publisher が止まっても controller は保存済みのセグメントを時刻だけで削除したり、公開範囲を自動的に取り下げたりはしない。
worker が最後の公開範囲を保持していれば、controller はその範囲のプレイリストを返し続ける場合がある。
新規映像の到着は [メトリクス](./observability.md) で監視する。

controller は 1 台でも leader 選出を行い、seed を省略しても後から peer が参加できる。
複数台での起動、DNS、交代時の状態消失、通信分断時の制約は [controller の冗長化](./cluster.md) を参照する。
症状からの切り分けは [運用](./operations.md#障害を切り分ける) を参照する。
