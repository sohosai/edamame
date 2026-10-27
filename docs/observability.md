# 可観測性

controller / worker / publisher は、OpenTelemetry のメトリクスを OTLP/gRPC で送信する。
controller と worker は、HTTP の `/health` でヘルスチェックに応答する。
publisher のトレース出力には対応していない。
ダッシュボード、アラートの閾値、通知先は利用する監視基盤で設定する。

## メトリクスの送信

送信先を指定した場合だけメトリクスの送信を有効にする。
アプリ自体は Prometheus 用の `/metrics` を提供しないため、Prometheus で収集する場合は OpenTelemetry Collector などで変換する。

- 送信方式は OTLP/gRPC とし、OTLP/HTTP には切り替わらない
- `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` を優先し、未設定の場合だけ `OTEL_EXPORTER_OTLP_ENDPOINT` を使う
- 選ばれた送信先が空の場合、または `OTEL_SDK_DISABLED=true` の場合は送信しない
- 初期化に失敗しても controller / worker / publisher の動作は継続し、標準エラーへ警告を出す
- アプリ終了時は未送信のメトリクスの送信を試みてから終了する

### 環境変数

| 環境変数 | 説明 |
| --- | --- |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | 共通の送信先。例：`http://127.0.0.1:4317`。metrics 専用の送信先が未設定の場合に使う |
| `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` | metrics 専用の送信先。空文字を指定すると、共通の送信先が設定されていても送信しない |
| `OTEL_SDK_DISABLED` | 前後の空白を除き、大文字小文字を区別せず `true` の場合に送信を無効にする |
| `OTEL_METRIC_EXPORT_INTERVAL` | 送信間隔をミリ秒で指定する。既定は `10000`。未設定、整数でない値、`100` 未満の値は既定値を使う |
| `OTEL_SERVICE_NAME` | `service.name` を上書きする。未設定または空の場合は `edamame-controller` / `edamame-worker` / `edamame-publisher` を使う |
| `EDAMAME_INSTANCE_ID` | controller と publisher の `service.instance.id` を指定する。worker では使わない |
| `OTEL_EXPORTER_OTLP_HEADERS` | 送信時のヘッダーを `key=value` のカンマ区切りで指定する |
| `OTEL_EXPORTER_OTLP_METRICS_HEADERS` | metrics 専用のヘッダー。指定時は `OTEL_EXPORTER_OTLP_HEADERS` より優先される |

送信先には scheme、ホスト、ポートを含める。
scheme がない場合は `http://` を補う。
ローカル確認で送信先を切り替える場合は、優先される `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` を設定する。

### リソース属性

| 属性 | 値 |
| --- | --- |
| `service.name` | `edamame-controller`、`edamame-worker`、`edamame-publisher` のいずれか。`OTEL_SERVICE_NAME` で上書きできる |
| `service.instance.id` | controller と publisher は下記の環境変数、worker は `--worker-id` の値 |
| `service.version` | アプリのパッケージバージョン |

controller の instance ID は、最初に設定されている `EDAMAME_INSTANCE_ID`、`HOSTNAME`、`COMPUTERNAME` の値から決める。
選ばれた値が空白だけの場合や、すべて未設定の場合は `controller` を使う。
publisher も同じ環境変数を使い、値がない場合は `publisher-<pid>` を使う。
複数の controller を監視する場合は、各機で異なる `EDAMAME_INSTANCE_ID` を指定する。
`env` などの環境識別用ラベルはアプリから送らない。

## メトリクス一覧

表の名前は OpenTelemetry で送信する名前である。
Gauge は現在の状態、Counter はプロセス内の累計を表し、再起動するとリセットされる。
controller と worker の状態を表す Gauge は 5 秒ごとに記録し、送信は `OTEL_METRIC_EXPORT_INTERVAL` の間隔で行う。
publisher の `up` は起動時と終了時に、`queue.depth` は publisher のループごとに記録する。

### controller

| メトリクス | 種類 | 単位 | ラベル | 説明 |
| --- | --- | --- | --- | --- |
| `edamame.controller.up` | Gauge | なし | なし | 記録時に `1` を出す |
| `edamame.controller.worker.last_sync_age` | Gauge | 秒 | `worker_id` | worker の最終同期からの経過時間 |
| `edamame.controller.worker.weight` | Gauge | なし | `worker_id` | worker の重み |
| `edamame.controller.channel.last_ingest_age` | Gauge | 秒 | `channel_id` | channel で最後にセグメントを新規保存してからの経過時間 |
| `edamame.controller.channel.target_duration` | Gauge | 秒 | `channel_id` | channel の設定されたセグメント長。HLS の整数の `#EXT-X-TARGETDURATION` とは異なり、ミリ秒を秒に換算した値 |
| `edamame.controller.channel.published_lag` | Gauge | 秒 | `channel_id` | 最終保存からの経過時間に、各 quality の最新番号と公開末尾の番号差の最大値 × セグメント長を加えた値 |
| `edamame.controller.segments.ingested` | Counter | なし | `channel_id`, `quality` | 新規に取り込んだセグメント数 |
| `edamame.controller.segments.published` | Counter | なし | `channel_id`, `quality` | 公開範囲の更新で新たに範囲へ入ったセグメント数 |
| `edamame.controller.errors` | Counter | なし | `operation` | `ingest` / `register` / `sync` / `external` の処理エラー数 |

worker と channel の Gauge は、controller が現在保持する登録情報と配信情報について記録する。
follower は配信用のリクエストを転送するため、leader と同じ worker / channel の Gauge があるとは限らない。
公開遅延は実際の映像の撮影時刻やプレイヤーの再生位置を測った値ではない。
まだ何も公開していない quality では、保持している先頭番号の 1 つ前を公開末尾として扱うが、番号が `0` の場合は `0` とする。

### worker

| メトリクス | 種類 | 単位 | ラベル | 説明 |
| --- | --- | --- | --- | --- |
| `edamame.worker.up` | Gauge | なし | なし | 記録時に `1` を出す |
| `edamame.worker.controller.last_success_age` | Gauge | 秒 | なし | controller への登録または sync が最後に成功してからの経過時間。初回成功前は worker の状態を初期化してからの経過時間 |
| `edamame.worker.cache.head_sequence` | Gauge | なし | `channel_id`, `quality` | キャッシュしている最新のセグメント番号 |
| `edamame.worker.cache.segments` | Gauge | なし | `channel_id`, `quality` | キャッシュしているセグメント数 |
| `edamame.worker.segments.downloaded` | Counter | なし | `channel_id`, `quality` | controller から取得してキャッシュに保存した回数 |
| `edamame.worker.segments.served` | Counter | なし | `channel_id`, `quality` | キャッシュからセグメントの成功応答を生成した回数。client が最後まで受信したことは保証しない |
| `edamame.worker.errors` | Counter | なし | `operation` | `register` / `sync` / `download` の処理エラー数 |

cache の Gauge は、キャッシュ内にセグメントがある channel / quality について記録する。
まだ処理が発生していない Counter や、対象が存在しない Gauge は、値が `0` の系列として取得できるとは限らない。

### publisher

| メトリクス | 種類 | 単位 | ラベル | 説明 |
| --- | --- | --- | --- | --- |
| `edamame.publisher.up` | Gauge | なし | なし | 起動時に `1`、正常終了時に `0` を記録する |
| `edamame.publisher.segments.encoded` | Counter | なし | `channel_id`, `quality` | encoder が publisher 側へ渡したセグメント数 |
| `edamame.publisher.segments.published` | Counter | なし | `channel_id`, `quality` | controller への upload に成功したセグメント数 |
| `edamame.publisher.upload.errors` | Counter | なし | `channel_id`, `quality`, `kind` | upload のエラー数。`kind` は `retryable` / `fatal` |
| `edamame.publisher.queue.depth` | Gauge | なし | `channel_id` | publish 待ち queue に残っているセグメントセット数 |

publisher の Counter は、セグメントの生成や upload が発生するまで系列が出ない場合がある。
`queue.depth` は publisher のループが動いている間だけ記録する。
encoder が起動直後に停止した場合など、queue の計測まで到達しないことがある。

### 監視するときの制約

controller と worker の `up` は稼働中に `1` を記録するが、停止時に `0` を送るものではない。
publisher の `up` は正常終了時に `0` を記録するが、強制終了などでは最後の値が残る。
Collector や監視基盤に最後の値が残ることがあるため、直近のサンプル時刻やプロセスの状態も確認する。
同期遅延と配信停止は、`last_sync_age`、`last_success_age`、`last_ingest_age`、`published_lag` を使って判断する。

## ヘルスチェック用の HTTP API

controller と worker は、HTTP リクエストを受け付けられる場合にステータス `200`、本文 `ok` を返す。
認証とメトリクスの送信設定は不要である。

| プロセス | パス |
| --- | --- |
| controller（パス接頭辞なし） | `GET /health` |
| controller（パス接頭辞あり） | `GET {path_prefix}/health`。接頭辞なしの `/health` は公開しない |
| worker | `GET /health` |
| worker（パス接頭辞あり） | `GET {path_prefix}/health` でも取得できる |

`path_prefix` は `--public-url` のパスから決まり、待ち受けアドレスは `--bind` で指定する。
worker が TLS を終端する構成では、同じ TLS の待ち受けで取得する。
`/health` は controller への登録、同期、証明書の信頼性、セグメントの配信可否を保証する readiness ではない。
controller の役割と leader の確認には、別の `GET /internal/cluster/health` を使う（[HTTP API](./api.md)）。

## ローカルでメトリクスを確認する

### Collector を起動する

リポジトリのルートで、[Compose 設定](../compose.observability.yaml) を使って Collector を起動する。
Docker Compose と、アプリのビルドに必要な Rust 環境が必要になる。
この構成は Grafana、Prometheus、Tempo を起動せず、メトリクスも永続化しない。

```bash
docker compose --env-file /dev/null -f compose.observability.yaml up -d
docker compose --env-file /dev/null -f compose.observability.yaml ps
```

公開するポートはホストの `127.0.0.1` に限定する。

| 用途 | アドレス |
| --- | --- |
| OTLP/gRPC の受信 | `127.0.0.1:4317` |
| OTLP/HTTP の受信 | `127.0.0.1:4318`。Edamame のアプリは使わない |
| Prometheus 形式の出力 | `http://127.0.0.1:8889/metrics` |

[Collector 設定](../otel/collector.yaml) は metrics のパイプラインだけを持つ。
出力する Prometheus メトリクスでは名前の `.` を `_` に置き換え、Counter に `_total`、秒単位のメトリクスに `_seconds` が付く。
`service.name` と `service.instance.id` は `job` と `instance` に使われ、設定によりリソース属性もラベルへ付加される。
ラベル名や単位の変換は Collector の設定に依存するため、実際の `/metrics` の出力で確認する。

### アプリを起動する

以下は bash の例であり、各プロセスを別のターミナルで起動する。
既存の送信先より metrics 専用の送信先を優先し、ローカルに送る。
既存のヘッダーはローカル確認に不要な認証情報を送らないよう解除する。
送信間隔を短くしても、Gauge の記録間隔は 5 秒のままである。

controller を起動する。

```bash
unset OTEL_EXPORTER_OTLP_HEADERS OTEL_EXPORTER_OTLP_METRICS_HEADERS
OTEL_EXPORTER_OTLP_METRICS_ENDPOINT=http://127.0.0.1:4317 \
OTEL_SDK_DISABLED=false \
OTEL_METRIC_EXPORT_INTERVAL=5000 \
  cargo run --locked -p edamame-controller -- \
    --bind 127.0.0.1:9000 \
    --public-url http://127.0.0.1:9000 \
    --storage-capacity 512MiB \
    --playlist-segments 6 \
    --sync-interval-ms 1000 \
    --sync-timeout-ms 10000 \
    --publish-wait-ms 2000 \
    --auth-token example-token \
    --cors-origin none
```

worker を起動する。

```bash
unset OTEL_EXPORTER_OTLP_HEADERS OTEL_EXPORTER_OTLP_METRICS_HEADERS
OTEL_EXPORTER_OTLP_METRICS_ENDPOINT=http://127.0.0.1:4317 \
OTEL_SDK_DISABLED=false \
OTEL_METRIC_EXPORT_INTERVAL=5000 \
  cargo run --locked -p edamame-worker -- \
    --worker-id worker-a \
    --bind 127.0.0.1:9001 \
    --public-url http://127.0.0.1:9001 \
    --controller-url http://127.0.0.1:9000 \
    --cache-limit 12 \
    --auth-token example-token \
    --cors-origin none
```

### 受信を確認する

アプリの起動ログに `OpenTelemetry metrics enabled` があることを確認する。
このログはメトリクス送信の初期化成功を示すが、Collector への送信成功を保証しないため、出力も確認する。

```bash
curl --fail --show-error http://127.0.0.1:9000/health
curl --fail --show-error http://127.0.0.1:9001/health
curl --fail --show-error http://127.0.0.1:8889/metrics
```

通常の既定値では送信は 10 秒間隔、上の例では 5 秒間隔になる。
起動直後に系列が出ない場合は、最初の送信と Collector の処理を待ってから取得し直す。
次の系列に値が出ることを確認する。

- `edamame_controller_up` と `edamame_worker_up` が `1` である
- `edamame_controller_worker_last_sync_age_seconds` に `worker_id="worker-a"` がある
- `edamame_worker_controller_last_success_age_seconds` の経過時間が継続的に増え続けず、同期に伴って小さい値へ戻る

セグメント数や channel の系列を確認する場合は、[導入手順](../README.md#動作を確認する) に従ってセグメントを送り、プレイリストと worker の取得を確認する。
ここでの手順はメトリクスの送受信確認であり、映像の再生確認ではない。

Prometheus を使う場合は、Prometheus 自身から到達できる Collector の `8889` 番ポートを収集先に設定する。
ホストで動く Prometheus は `127.0.0.1:8889` を使えるが、別コンテナの `127.0.0.1` はそのコンテナ自身を指す。
Grafana のデータソース、ダッシュボード、通知は別途設定する。

確認が終わったら、アプリを `Ctrl+C` で停止してから Collector を停止する。

```bash
docker compose --env-file /dev/null -f compose.observability.yaml down
```

## データが届かないとき

- metrics 専用と共通の送信先を両方確認する。metrics 専用の値が優先され、空文字でも共通の値には戻らない
- `OTEL_SDK_DISABLED=true` になっていないか確認する
- 送信先が OTLP/gRPC を受け付けるか確認する。ローカル構成では `4317` を使い、`4318` には送らない
- アプリの標準エラーに `[telemetry]` の警告が出ていないか確認する
- `docker compose --env-file /dev/null -f compose.observability.yaml logs otel-collector` で Collector の起動失敗や受信エラーを確認する
- `4317` / `4318` / `8889` 番ポートが既存プロセスと競合していないか確認する
- `/metrics` に系列があって監視画面にない場合は、収集先へ接続できるかと監視基盤側のラベル、収集間隔、表示時間範囲を確認する
- プロセス停止後も最後の値が残る場合は、Collector の保持設定と監視基盤の古いサンプルの扱いを確認する
