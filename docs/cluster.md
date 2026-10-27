# controller の冗長化

## 構成の考え方

複数の controller は leader を選ばない。
各 controller は PUT、GET、worker の登録と sync、プレイリスト生成を自分のメモリで処理する。
publisher が全 controller に同じセグメントを送り、worker の登録とキャッシュ報告は heartbeat で controller 間を共有する。
client は 1 台の controller からプレイリストを取り、segment は worker から取得する。

この構成は controller 間でセグメントや採番を複製しない。
publisher の fan-out によって同じ入力が各 controller に入るが、controller の再起動や通信分断で状態がずれる可能性は残る。
無停止の配信や、全 controller で同一の状態を保証するものではない。

## 起動と DNS の設定

全 controller に同じ `--auth-token` と、機体ごとに異なる `--public-url` を指定する。
2 台をローカルで確認する場合は、リポジトリのルートから別のターミナルで起動する。

```bash
cargo run --locked -p edamame-controller -- \
  --bind 127.0.0.1:9000 --public-url http://127.0.0.1:9000 \
  --seed http://127.0.0.1:9002 \
  --storage-capacity 512MiB --playlist-segments 6 \
  --sync-interval-ms 1000 --sync-timeout-ms 10000 --publish-wait-ms 2000 \
  --auth-token example-token --cors-origin '*'
```

```bash
cargo run --locked -p edamame-controller -- \
  --bind 127.0.0.1:9002 --public-url http://127.0.0.1:9002 \
  --seed http://127.0.0.1:9000 \
  --storage-capacity 512MiB --playlist-segments 6 \
  --sync-interval-ms 1000 --sync-timeout-ms 10000 --publish-wait-ms 2000 \
  --auth-token example-token --cors-origin '*'
```

```bash
curl --fail --show-error http://127.0.0.1:9000/internal/cluster/health
curl --fail --show-error http://127.0.0.1:9002/internal/cluster/health
```

`/internal/cluster/health` は `public_url` と、heartbeat で知っている member の配列を返す。
leader、metric、役割は返さない。

本番運用での DNS と接続先の設定は次のとおりである。

- 共有名（例：`controller.example.test:9000`）を DNS ラウンドロビンで複数の A レコードに対応させ、全機で待ち受けポートと API のパスをそろえる
- 各機の `--public-url` と `--seed` には共有名ではなく機体固有の名前を使う
- worker の `--controller-url` には、接続できる controller を優先順に複数指定する
- publisher の `[[controller]]` には、全 controller を指定する。publisher は全台へ並列に送る
- client のプレイリスト URL は共有名または 1 台の controller に向ける。live2025-server の `candidate_live_urls` を使う場合は、server が選んだ controller を client が使う
- 停止した機体の A レコードは削除する。TTL を短くしても既存の DNS キャッシュと TCP 接続が残るため、接続失敗を再試行する

## heartbeat と member の発見

各 controller は 1 秒ごとに既知の全 member へ並行して heartbeat を送る。
リクエストの timeout は 1 秒とし、直接疎通できてから 3 秒未満の member を生存とみなす。
認証済みの heartbeat を受信したときも送信元の生存時刻を更新する。
heartbeat は共通の Bearer token で認証する。

seed は最初の接触先であり、全 controller を列挙する必要はない。
heartbeat の送受信で既知の member を交換し、発見した URL にも直接接触する。
間接的に届いた member 情報だけでは、その member の生存時刻を更新しない。
発見した member は死亡後も表から削除せず、再接触を続ける。

## worker registry の共有

worker は `PUT /workers/{worker_id}` と `POST /workers/{worker_id}/sync` を、現在接続している 1 台の controller に送る。
controller は自分の worker の状態（公開 URL、weight、キャッシュ済み、取得中）を heartbeat に含める。
heartbeat を受け取った controller は、その worker 状態を自分の registry に反映する。

これにより、worker が A に登録しただけでも、B は次の heartbeat で同じ worker を配信先として使える。
worker が sync した controller だけが `fetch_segments` を返し、ほかの controller は worker のキャッシュ報告だけを受け取る。
受信側では最終同期時刻を heartbeat の受信時刻として扱う。
そのため、heartbeat を取りこぼした controller では `--sync-timeout-ms` を過ぎると一時的に worker を配信先から外すが、次の heartbeat で戻る。

worker の weight も heartbeat で共有される。
weight が変わると、各 controller は配信先選択の現在値をリセットする。

## worker の controller 切り替え

worker は `--controller-url` を複数指定できる。
指定順に接続し、現在の controller に登録または sync できなくなったら次の controller へ切り替える。

- 起動直後は、現在の controller に sync を試す。worker が未登録なら、登録してから sync する
- 登録に成功すると、worker のキャッシュを捨てて controller から取り直す
- 接続失敗、timeout、5xx では次の controller へ移る。register と sync の timeout は 5 秒、接続 timeout は 2 秒とする
- sync が 404 の場合は、controller の再起動などで registry が失われている。同じ controller に再登録し、キャッシュを捨てて取り直す
- 切り替え先の controller が heartbeat で worker をすでに知っている場合は、登録をやり直さずに sync できる。この場合はキャッシュを保持する
- 400 や 401 は設定エラーとして扱い、現在の controller を再試行する

cache を捨てるのは、controller の採番やセグメント内容がリセットされた可能性がある再登録時だけである。
正常に heartbeat を共有している controller への切り替えでは、キャッシュをそのまま使う。

## publisher の fan-out

publisher は `[[controller]]` に指定した全 controller へ、同じセグメントを並列に PUT する。
1 台の controller への PUT が遅い、または失敗しても、ほかの controller への送信は継続する。
controller ごとに 5 秒の PUT timeout を設け、超えた controller はそのチャンネルの fan-out 対象から外す。
Fatal（400、401、409）はチャンネルを停止する。

複数 controller の sequence をそろえるため、publisher は起動時に全 controller の次のセグメント番号を読み、その最大値から始める。
1 セグメントを全 controller に送ってから次のセグメントへ進む。
ただし、timeout や通信分断で一部の controller だけが欠けることはある。

`--stream-credentials-url` に登録する `candidate_live_urls` は、全 controller の `live_url` を含む。
live2025-server はその中から 1 つを選び、client は選ばれた controller を使う。
client は controller 間の負荷分散を行わない。
segment は worker から取得するため、worker の weight による配信先の分散は維持される。

## client の取得

client は 1 台の controller から playlist を取得する。
live2025-server は `candidate_live_urls` の中から利用可能な controller を 1 つ選び、client は失敗時に次の候補へ fallback する。
controller 間の 307 は存在しない。
worker は playlist に書かれた公開 URL から segment を取得する。

## 再起動と失敗時の扱い

controller のセグメント、採番、channel / quality の属性、暗号化指定、worker の登録、外部配信の登録はメモリにだけ保持する。
controller を再起動すると、その controller の状態は失われる。

- worker の登録は、ほかの controller の heartbeat から戻る
- セグメントと採番は戻らない。publisher の fan-out は再起動後の新しいセグメントだけで、過去分の backfill はしない
- worker は起動中の controller に sync を続ける。再起動した controller に接続した場合は 404 を受けて再登録し、キャッシュを捨てて取り直す
- client は live2025-server の別の候補へ fallback する
- 再起動した controller を client の候補に戻す場合は、新しい publisher の fan-out が届いていることを確認する

publisher の fan-out 中に controller が落ちた場合、その controller のセグメントだけ欠けることがある。
成功した controller はそのまま配信を続け、欠けた controller は live2025-server の health probe で除外されるか、client の fallback で回避する。
controller 間で状態を自動的に補完しない。

quorum と fencing は使わない。
通信分断時は各 controller が独立して動くため、同じ channel の採番や外部配信の登録が controller 間でずれることがある。
client は 1 controller だけを使い、分断された controller を混ぜない運用にする。

## 旧 leader 構成からの移行

leader 選出と 307 転送を持つ旧構成とは互換性がない。
controller、worker、publisher を新しい構成へ同時に切り替える。
worker は新しい controller の一覧へ、publisher は `[[controller]]` へ全 controller を設定する。
