# controller の冗長化

## 起動と DNS の設定

単体の設定一覧は [controller](./controller.md#設定) を参照する。
全 controller に同じ `--auth-token` と異なる `--public-url` を指定する。
2 台をローカルで確認する場合は、リポジトリのルートから別のターミナルで起動する。

```bash
cargo run --locked -p edamame-controller -- \
  --bind 127.0.0.1:9000 --public-url http://127.0.0.1:9000 \
  --metric 10 --seed http://127.0.0.1:9002 \
  --storage-capacity 512MiB --playlist-segments 6 \
  --sync-interval-ms 1000 --sync-timeout-ms 10000 --publish-wait-ms 2000 \
  --auth-token example-token --cors-origin '*'
```

```bash
cargo run --locked -p edamame-controller -- \
  --bind 127.0.0.1:9002 --public-url http://127.0.0.1:9002 \
  --metric 20 --seed http://127.0.0.1:9000 \
  --storage-capacity 512MiB --playlist-segments 6 \
  --sync-interval-ms 1000 --sync-timeout-ms 10000 --publish-wait-ms 2000 \
  --auth-token example-token --cors-origin '*'
```

```bash
curl --fail --show-error http://127.0.0.1:9000/internal/cluster/health
curl --fail --show-error http://127.0.0.1:9002/internal/cluster/health
```

選出が完了すると metric `10` の機体が leader、`20` の機体が follower になる。
この構成は状態を複製しないため、冗長化しても無停止の配信やデータの保持を保証しない。

本番運用での DNS と接続先の設定、および障害時の注意点は次のとおりである。

- 共有名（例：`controller.example.test:9000`）を DNS ラウンドロビンで複数の A レコードに対応させ、全機で待ち受けポートと API のパスをそろえる
- 各機の `--public-url` と `--seed` には共有名ではなく機体固有の名前を使う
- worker と publisher の `--controller-url`、client のプレイリスト URL は共有名に向ける（GET と sync の `307` に追従でき、転送先の固有 URL にも到達できることが必要になる）
- 共有名を用意しない場合は、worker の `--controller-url` と publisher の `[[controller]]` に各機の固有 URL を複数指定する
- 停止した機体の A レコードは削除する。
  TTL を短くしても既存の DNS キャッシュと TCP 接続が残るため、接続失敗を再試行する
- worker のリクエストは最大 30 秒待つ場合があり、復旧は leader の選出完了より遅れることがある。
  leader 未確定や中継失敗の `503` では `Retry-After: 1` に従う

## 役割と保存する状態

controller は台数によらず常に cluster の選出処理を使う。
各 controller には固有の `--public-url` と、peer 間で共通の `--auth-token` を必ず指定する。
`--metric` の既定値は `100` とし、複数台で起動する場合は必要に応じて優先度を変える。
起動例と DNS の設定は [この文書の起動と DNS の設定](#起動と-dns-の設定) を参照する。
seed を指定しない場合は自分 1 台で始まり、最初の選出で自分が leader になる。
公開 URL または token がなければ起動に失敗する。
seed がなくても認証済みの heartbeat を受け付け、後から参加した peer と選出を続ける。

leader はセグメントファイル、採番、channel と quality のメタデータ、暗号化の指定、外部配信の登録、worker の登録とキャッシュ報告をメモリに保存する。
follower はこれらを保存せず、leader への転送だけを行う。
状態の複製と永続化は行わない。

各 controller は自分の役割、leader の URL、既知の member を管理する。
member は public URL、metric、役割、直接疎通に成功した最終時刻を持つ。
役割と leader URL は `GET /internal/cluster/health` で確認できる。
この API は認証なしで 200 を返すため、200 だけでは leader と判定しない。

## heartbeat と member の発見

各 controller は 1 秒ごとに既知の全 member へ並行して heartbeat を送る。
リクエストの timeout は 1 秒とし、直接疎通できてから 3 秒未満の member を生存とみなす。
認証済みの heartbeat を受信したときも送信元の生存時刻を更新する。
heartbeat は共通の Bearer token で認証する。

seed は最初の接触先であり、全 controller を列挙する必要はない。
heartbeat の送受信で既知の member を交換し、発見した URL にも直接接触する。
間接的に届いた member 情報だけでは、その member の生存時刻や確定済みの metric を更新しない。
発見した member は死亡後も表から削除せず、再接触を続ける。

## leader の判定

自分と生存している member の中で、metric が最小のものを leader にする。
metric は u32 で、小さい値を優先する。publisher の `[[controller]]` の metric と同じ向きである。
省略時は `100`、明示的に指定できる範囲は `0` 以上 `4294967295` 以下とする。
metric が同じ場合は、正規化後の public URL の文字列を比較して大きい方を選ぶ。
各 controller は同じ URL を自分と同じインスタンスとして扱うため、public URL を共有しない。

cluster の起動直後は follower とし、最初の heartbeat の送信が 1 回分完了するまで leader URL を確定しない。
この間の配信用 API は `503 Service Unavailable` と `Retry-After: 1` を返す。
接触先が空なら追加の待ち時間は設けず、最初の選出で自分を leader にする。
seed がすべて停止していても、初回の送信が完了すれば自分を leader にできる。

上位機が停止すると、その member の最終成功から 3 秒以上経過した後の判定で、生存している最小の metric の機体が leader になる。
上位機が復帰すると再び上位機を leader にする。
障害検知と通信の時間があるため、切り替えの所要時間は正確に 3 秒とは限らない。

## リクエストの転送

`/health`、`/internal/cluster/health`、`/internal/cluster/heartbeat` は各インスタンス自身で処理する。
他の API は次の規則で処理する。

| 役割とリクエスト | 動作 |
| --- | --- |
| leader | 従来どおり処理する |
| follower への GET | leader へ 307 を返す。`Cache-Control: no-store` と設定に応じた読み取り用 CORS ヘッダーを付ける |
| follower への sync の POST | leader へ 307 を返す。`Cache-Control: no-store` を付け、POST と body を保持する |
| follower への認証付き PUT | follower で認証した後、leader へ内部中継する |
| leader URL が未確定 | 503 と `Retry-After: 1` を返す。PUT は先に認証する |
| PUT の中継に失敗 | 503 と `Retry-After: 1` を返し、heartbeat による再確認をすぐ要求する |

Location は leader の public URL に元の API の path と query を連結する。
public URL に path の接頭辞がある場合、各 controller はその接頭辞の下で API を公開する。
転送時は受信側の接頭辞を外し、leader 側の接頭辞を付ける。
例えば `http://a.test/a/segments/live/720p/0.ts?x=1` は `https://b.test/b/segments/live/720p/0.ts?x=1` に転送される。
HTTP と HTTPS の両方を転送先に指定できる。

PUT は別ホストへの redirect で Authorization が失われるため、307 を使わない。
中継では method、path、query、Authorization、Content-Type、body を渡し、leader の status、応答ヘッダー、body を返す。
受信 body は最大 64 MiB をメモリに保持し、leader では各 API 本来の上限も適用する。
中継の timeout は 10 秒とする。
中継に失敗しても PUT を自動再送しないため、publisher は失敗した PUT を再試行する。
leader で保存できた後に応答だけが失われた場合も、同じ識別子の再 PUT で処理できる。
ただし、交代を挟むと以前の採番は失われる。

worker と publisher は既定の redirect 追従を使う。
worker は自身の配信に TLS を使わない場合でも、HTTPS への redirect を追従できる。
controller も TLS を終端できる。`--tls-cert` / `--tls-key` か `--acme` を指定した場合、`--public-url` は HTTPS にする（[TLS 配信](./controller.md#tls-配信)）。
TLS を使わない場合は、HTTPS の URL を指定しても TLS 接続は受け付けられないため、前段で終端する。
TLS を使う場合、peer は `--public-url` へ HTTPS で heartbeat を送るため、各機が証明書を検証できる必要がある。

## 交代時の状態と復旧

leader から follower に降格すると、処理中のリクエストの完了を待ってから、保存していた状態を全消去する。
古い採番やセグメントファイルを、次の昇格後に持ち越さない。
新しい leader は空の状態から始まり、セグメント番号は 0 からになる。
暗号化の指定、外部配信の登録、worker の登録も失われるため、publisher と worker が登録し直す。

worker は sync で 404 を受け取ると再登録する。
登録できた時点で古いキャッシュを捨て、新しい leader から取得し直す。
client はプレイリストを取り直し、0 から始まる media sequence に再同期する。
交代時は数秒間プレイリストが 404 または 503 になり得る。
停止機体への既存 TCP 接続が応答しない場合は、worker のリクエスト timeout が最大 30 秒であるため、worker の再登録と再取得がさらに遅れる場合がある。
上位機への切り戻しでも同じ中断が起きる。

## 二重 leader と PUT の取りこぼし

quorum と fencing は使わない。
ネットワークが分断された場合、それぞれの側が自分の見える生存機から leader を選ぶため、複数の leader が発生する。
片方向だけ疎通できる場合、heartbeat の遅延や処理中のリクエストで応答が遅れる場合、交代直後で認識が揃わない場合も、一時的に二重 leader になり得る。

二重 leader の間に受け付けた PUT は、降格時に破棄される可能性がある。
切り替え直前に旧 leader へ送った PUT も、新 leader に複製されない。
成功した PUT を交代後も保持する保証や、全リクエストで共通の採番を続ける保証はない。
通信が回復すれば最小の metric の機体へ戻るが、分断中の状態は統合しない。
