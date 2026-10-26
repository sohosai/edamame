# worker

worker は controller からセグメントファイルを取得し、メモリ上のキャッシュから client へ配信する。
プレイリストの生成、公開範囲の決定、配信先の選択は controller が担当し、worker は登録と同期で自身の設定とキャッシュ状況を報告する。

## 必要な環境とネットワーク条件

- ソースから起動する場合は、[導入手順](../README.md) に記載した Rust 環境を用意する
- worker から `--controller-url` と、follower が転送先として返す leader の公開 URL へ接続できること
- client から `--public-url` へ接続できること。
  `--bind` は待ち受けアドレスであり、公開 URL ではない
- controller と同じ認証トークンを用意し、worker ごとに異なる `--worker-id` を指定すること

初期接続先の `--controller-url` は `http` の絶対 URL に限る。
一方、leader へのリダイレクトでは `https` にも追従できる。
worker 自身の TLS 設定は client 向けの通信に適用され、controller への初期接続を HTTPS にするものではない。
controller との内部通信は信頼できるネットワーク内に置く。

## 最小の起動

リポジトリのルートで実行する。
次の例は、同じホストの `http://127.0.0.1:9000` で controller が待ち受け、認証トークンが `example-token` の場合である。
別ホストの client から利用する場合は、待ち受けアドレスと公開 URL を環境に合わせる。

```bash
cargo run --locked -p edamame-worker -- \
  --worker-id worker-a \
  --bind 127.0.0.1:9001 \
  --public-url http://127.0.0.1:9001 \
  --controller-url http://127.0.0.1:9000 \
  --cache-limit 12 \
  --auth-token example-token \
  --cors-origin none
```

controller より先に起動しても、登録に成功するまで再試行する。
`--cors-origin none` はブラウザの別 origin からの読み取りを許可しない設定である。
ブラウザで再生する場合は、プレイヤーを配信する origin を許可する。

## CLI と環境変数

コマンドライン引数と環境変数の両方を指定した場合は、コマンドライン引数を優先する。
表の「なし」は既定値がなく、任意項目ではその機能や値を設定しないことを表す。

| オプション | 環境変数 | 必須条件 | 既定値 | 内容と制約 |
| --- | --- | --- | --- | --- |
| `--worker-id` | `EDAMAME_WORKER_ID` | 必須 | なし | 登録する識別子。`[A-Za-z0-9_-]` の 1〜64 文字 |
| `--bind` | `EDAMAME_BIND` | 必須 | なし | HTTP または TLS の待ち受け IP アドレスとポート |
| `--public-url` | `EDAMAME_PUBLIC_URL` | 必須 | なし | client 向けの `http` / `https` 絶対 URL。TLS / ACME 指定時は `https` が必須 |
| `--controller-url` | `EDAMAME_CONTROLLER_URL` | 必須 | なし | worker から接続する `http` の絶対 URL |
| `--cache-limit` | `EDAMAME_CACHE_LIMIT` | 必須 | なし | `(channel_id, quality)` ごとの保持件数。1 以上の整数で、実行環境の `usize` に収まる値 |
| `--weight` | `EDAMAME_WEIGHT` | 任意 | `1` | 配信先に選ぶ回数の相対的な重み。1〜4294967295 の整数 |
| `--auth-token` | `EDAMAME_AUTH_TOKEN` | CLI では任意、controller への登録には必須 | なし | controller と同じ Bearer トークン。空白と制御文字を除く表示可能 ASCII。登録の PUT にだけ付ける |
| `--cors-origin` | `EDAMAME_CORS_ORIGIN` | 任意 | CORS 無効 | 許可する origin をカンマ区切りで指定する。`*` と `none` はそれぞれ単独で指定する。省略時は警告を出す |
| `--tls-cert` | `EDAMAME_TLS_CERT` | `--tls-key` 使用時は必須 | なし | 証明書チェーンの PEM ファイル。`--acme` と排他 |
| `--tls-key` | `EDAMAME_TLS_KEY` | `--tls-cert` 使用時は必須 | なし | 秘密鍵の PEM ファイル。`--acme` と排他 |
| `--acme` | `EDAMAME_ACME` | 任意 | `false` | ACME で証明書を取得する。環境変数は `true` / `false`。静的 PEM の指定と排他 |
| `--acme-directory` | `EDAMAME_ACME_DIRECTORY` | 任意、指定時は `--acme` が必須 | `https://acme-v02.api.letsencrypt.org/directory` | ACME ディレクトリの HTTPS URL。fragment は不可 |
| `--acme-cache-dir` | `EDAMAME_ACME_CACHE_DIR` | `--acme` 使用時は必須。指定時も `--acme` が必須 | なし | アカウント鍵と証明書の保存先。書き込み可能な永続ディレクトリを指定する |
| `--acme-contact` | `EDAMAME_ACME_CONTACT` | 任意、指定時は `--acme` が必須 | なし | 連絡先メールアドレス。省略時は警告を出す |

公開 URL と controller URL は query と fragment を許可せず、末尾の `/` を取り除いて使う。
公開 URL にパスを含めると、worker はそのパスを接頭辞とする `/segments/...` を待ち受ける。
例えば `https://edge.example.test/worker-a` なら `/worker-a/segments/...` となり、前段のプロキシはこの接頭辞を保持して転送する。
公開 URL の scheme を `https` に変えるだけでは、worker の TLS は有効にならない。

## 登録と同期

worker は起動後に登録し、その後は同期を繰り返す。

1. `PUT /workers/{worker_id}` で公開 URL と weight を登録する。
   失敗した場合はログを出し、1 秒待って再試行する。
   トークンの省略や不一致による `401` でも終了せず、再試行を続ける。
2. 登録に成功すると手元のキャッシュをすべて捨て、レスポンスの `sync_interval_ms` を同期の待ち時間として設定する。
3. `POST /workers/{worker_id}/sync` にキャッシュ済みと取得中の一覧を送り、`fetch_segments` で不足分の取得指示を受ける。
   取得したセグメントは後続の同期で報告し、controller が公開範囲と配信先を判断する。
4. 各同期の処理後に `sync_interval_ms` だけ待ち、手順 3 に戻る。
   リクエストに時間がかかると送信間隔も延びる。

controller 側の公開条件は [controller の公開範囲の決定](./controller.md#公開範囲の決定)、JSON とステータスは [HTTP API](./api.md) を参照する。

### 取得と再試行

セグメントの取得と失敗時の再試行は次の規則で動く。

- 取得バッチは同時に 1 つだけ実行し、バッチ内のセグメントを順番に GET する。
  取得中も同期は続けるが、新しい取得指示に着手するのは実行中のバッチが終わった後である
- 登録、同期、セグメント取得の HTTP リクエストには、それぞれ 30 秒のタイムアウトを設定する。
  遅い取得があると後続のセグメントも待つため、同期できていてもキャッシュが最新とは限らない
- セグメントの取得や読み取りに失敗した場合はログを出し、そのバッチの次のセグメントへ進む。
  失敗した分は、後続の同期で再び取得を指示された場合に取り直す
- 同期の通信失敗や `404` 以外のエラーではキャッシュを保持し、待ち時間の後に同期を再試行する
- 同期で `404` を受け取ると、取得中のタスクを止めて再登録する。
  キャッシュ全体を捨てるのは再登録に成功した時点であり、登録に失敗している間は既存キャッシュの配信を続ける
- controller の再起動や leader 交代による採番リセットでも、この再登録と取り直しで追従する（交代時の状態消失と接続失敗の制約は [controller の冗長化](./cluster.md) を参照する）

## キャッシュと配信比率

キャッシュの保持と削除は次の規則で動く。

- キャッシュはメモリ上だけに保持し、worker を再起動すると失われる
- `--cache-limit` は `(channel_id, quality)` ごとのセグメント件数であり、上限を超えると番号の小さいものから削除する
- 取得した古い番号が保存直後に上限超過で削除された場合は、その channel / quality のキャッシュをすべて捨てて取り直せるようにする
- client が要求したセグメントがキャッシュにない場合は `404` を返し、その場で controller へ取りに行くことはない。
  controller との通信が途切れても、キャッシュ済みのセグメントは引き続き配信する
- この上限はバイト数でも、worker 全体のメモリ上限でもない。
  channel / quality の数、セグメントのサイズ、取得中のデータ、配信中に参照されるデータによって実際の使用量が変わる
- キャッシュ上限は controller の `--playlist-segments` より 1 以上大きくすることを推奨する（容量と同期の調整は [パラメータ設計とメモリ見積もり](./operations.md#パラメータ設計とメモリ見積もり) を参照する）

`--weight` の扱いは次のとおりである。

- 公開範囲をすべて持つ候補間でメディアプレイリストの配信先に選ばれる回数の比を決める（例えば重みが `4` と `1` なら、候補が変わらない間はその比率で選ぶ）
- 帯域や同時接続数を直接制限する設定ではなく、CPU 使用率などに応じて自動調整されることもない
- 変更は worker の再起動と再登録で反映する

## TLS 配信

TLS を前段のプロキシで終端する場合は worker を HTTP で動かし、`--public-url` に client 向けの HTTPS URL を指定する。
worker 自身で終端する場合は、静的 PEM または ACME のどちらかを使う。
両方式は同時に指定できず、公開 URL は HTTPS が必須になる。

### 静的 PEM

証明書と対応する秘密鍵を読み込み、待ち受けに成功すると controller への登録を始める。
公開ルート CA での信頼性、公開 URL とのホスト名の一致、有効期限を登録条件として検査するわけではない。
これらは client が接続できるように運用者が確認する。

次の例では、`edge.example.test` を実際の配信ホスト名に置き換え、そのホスト名を含む証明書チェーンと秘密鍵を指定のパスに用意する。
実行ユーザーが PEM を読めること、client が対象ホストの TCP `9443` 番へ到達できることを前提とする。
controller は同じホストの HTTP `9000` 番で動かす。

```bash
cargo run --locked -p edamame-worker -- \
  --worker-id worker-tls \
  --bind 0.0.0.0:9443 \
  --public-url https://edge.example.test:9443 \
  --controller-url http://127.0.0.1:9000 \
  --cache-limit 12 \
  --auth-token example-token \
  --cors-origin none \
  --tls-cert /etc/edamame/fullchain.pem \
  --tls-key /etc/edamame/privkey.pem
```

証明書と鍵の更新時刻を 60 秒ごとに確認し、変更時に再起動なしで読み直す。
読み直しに失敗した場合は古い設定を維持してログを出し、次の確認で再試行する。
期限切れになっても自動で登録や同期を止める仕組みはない。

### ACME

ACME は TLS-ALPN-01 で公開 URL のホスト名に対する証明書を取得し、更新する。
公開 DNS に対象ホストを登録し、インターネットからの TCP `443` 番が worker の TLS 待ち受けへ届く必要がある。
前段で TLS を終端するとチャレンジを受け取れないため、ポート転送では TLS と ALPN をそのまま通す。
HTTP `80` 番は使わず、IP アドレスを証明書の対象には指定できない。

次の例では、実際に管理する公開ホスト名と連絡先に置き換え、TCP `443` 番を worker の `8443` 番へ転送する。
実行ユーザーが書き込める `./acme-cache` を、再起動やコンテナ再作成で失わない場所に置く。

```bash
cargo run --locked -p edamame-worker -- \
  --worker-id worker-acme \
  --bind 0.0.0.0:8443 \
  --public-url https://edge.example.test \
  --controller-url http://127.0.0.1:9000 \
  --cache-limit 12 \
  --auth-token example-token \
  --cors-origin none \
  --acme \
  --acme-cache-dir ./acme-cache \
  --acme-contact operator@example.test
```

ACME での動作と制約は次のとおりである。

- 配信中の証明書をホスト名と公開ルート CA で検証できる間だけ登録と同期を行う（ステージング CA の証明書はこの検証を通らず、登録されない）
- 証明書の変更時と 60 秒ごとの確認で検証に失敗すると同期を止め、controller が同期タイムアウト後に配信先から外す。
  有効な証明書に戻ると再登録し、キャッシュを捨てて取り直す
- 証明書取得に失敗しても再試行し、取得済みの証明書が有効なら使い続ける
- EAB が必要な ACME サービスには対応していない
- キャッシュには秘密鍵が含まれるためアクセスを制限し、Unix ではディレクトリの権限を `0700` に設定する

## 動作確認と監視

`GET /health` は HTTP の応答可否だけを示し、controller への登録や配信可能なキャッシュの存在を保証しない。
公開 URL にパスがある場合は、その接頭辞付きの `/health` も利用できる。
同期の失敗と取得失敗は標準エラーへ出力する。
登録状況、同期の経過時間、キャッシュ件数の確認には [可観測性](./observability.md) を使う。
