# 可観測性

Edamame の controller / worker / publisher は OpenTelemetry metrics を実装済みです。
controller / worker は health endpoint も提供します。traces は Phase 2 で検討します。

## 現在の構成

```text
edamame-controller ─┐
                    ├─ OTLP/gRPC ─> OTel Collector ─> Prometheus ─> Grafana
edamame-worker ─────┤                 (iromono)
edamame-publisher ──┘
```

- controller / worker / publisher は OTLP/gRPC で metrics を送る
- OTel Collector は Prometheus exporter で metrics を公開する
- Prometheus の `edamame` job が Collector を scrape する
- Prometheus の `edamame-health` job が blackbox_exporter 経由で controller / worker の `/health` を probe する
- Grafana の `Edamame` フォルダで可視化する
- 通知は既存の Discord contact point `infra 死活監視` を使う
- traces はまだない。publisher metrics は導入済み

本番の送信先:

```text
OTEL_EXPORTER_OTLP_ENDPOINT=http://iromono:4317
```

Grafana:

```text
http://iromono:3000/d/edamame-overview
```

## メトリクス

### Controller

| メトリクス | 種類 | ラベル | 説明 |
| --- | --- | --- | --- |
| `edamame.controller.up` | Gauge | なし | controller の稼働状態 |
| `edamame.controller.worker.last_sync_age` | Gauge (s) | `worker_id` | worker の最終同期からの経過時間 |
| `edamame.controller.worker.weight` | Gauge | `worker_id` | worker の重み |
| `edamame.controller.channel.last_ingest_age` | Gauge (s) | `channel_id` | channel の最終セグメント受信からの経過時間 |
| `edamame.controller.channel.target_duration` | Gauge (s) | `channel_id` | channel のセグメント長 |
| `edamame.controller.channel.published_lag` | Gauge (s) | `channel_id` | 公開遅延。最終受信からの経過時間と未公開セグメント数から算出する |
| `edamame.controller.segments.ingested` | Counter | `channel_id`, `quality` | 新規に取り込んだセグメント数 |
| `edamame.controller.segments.published` | Counter | `channel_id`, `quality` | 新規に公開したセグメント数 |
| `edamame.controller.errors` | Counter | `operation` | controller のエラー数。`operation` は `ingest` / `register` / `sync` / `external` |

### Worker

| メトリクス | 種類 | ラベル | 説明 |
| --- | --- | --- | --- |
| `edamame.worker.up` | Gauge | なし | worker の稼働状態 |
| `edamame.worker.controller.last_success_age` | Gauge (s) | なし | controller への登録または sync が最後に成功してからの経過時間 |
| `edamame.worker.cache.head_sequence` | Gauge | `channel_id`, `quality` | キャッシュしている最新セグメントの sequence |
| `edamame.worker.cache.segments` | Gauge | `channel_id`, `quality` | キャッシュしているセグメント数 |
| `edamame.worker.segments.downloaded` | Counter | `channel_id`, `quality` | controller から取得したセグメント数 |
| `edamame.worker.segments.served` | Counter | `channel_id`, `quality` | 視聴者へ配信したセグメント数 |
| `edamame.worker.errors` | Counter | `operation` | worker のエラー数。`operation` は `register` / `sync` / `download` |

### Publisher

| メトリクス | 種類 | ラベル | 説明 |
| --- | --- | --- | --- |
| `edamame.publisher.up` | Gauge | なし | publisher の稼働状態 |
| `edamame.publisher.segments.encoded` | Counter | `channel_id`, `quality` | encode したセグメント数 |
| `edamame.publisher.segments.published` | Counter | `channel_id`, `quality` | controller への upload に成功したセグメント数 |
| `edamame.publisher.upload.errors` | Counter | `channel_id`, `quality`, `kind` | upload のエラー数。`kind` は `retryable` / `fatal` |
| `edamame.publisher.queue.depth` | Gauge | `channel_id` | publish 待ち queue の深さ |

publisher は最初の導入として、稼働状態・encode / publish 数・upload エラー・queue 深さを送ります。
publish 成功からの経過時間、encode / upload 時間、ffmpeg の再起動回数は Phase 2 で追加します。

Prometheus 上では、OTel の counter に `_total` が付き、unit `s` は `_seconds` になります。
例えば `edamame.worker.cache.segments` は `edamame_worker_cache_segments`、`edamame.controller.errors` は `edamame_controller_errors_total` になります。

### リソース属性

| 属性 | Prometheus での扱い |
| --- | --- |
| `service.name` | Collector が `job` に変換する |
| `service.instance.id` | Collector が `instance` に変換する |
| `service.version` | `service_version` として残る |
| `env` | Prometheus の scrape 設定で `dev` を付与する |

worker の `service.instance.id` は `worker_id` です。
controller と publisher の `service.instance.id` は `EDAMAME_INSTANCE_ID`、`HOSTNAME`、`COMPUTERNAME` の順で決まります。
publisher はどれもない場合、`publisher-<pid>` になります。

## Health endpoint

controller と worker は、死活監視用の HTTP endpoint を提供します。

| プロセス | パス | 応答 |
| --- | --- | --- |
| controller | `GET /health` | `200 ok` |
| worker | `GET /health` | `200 ok` |
| worker（path prefix あり） | `GET {path_prefix}/health` | `200 ok` |

- 認証は不要です。プロセスが HTTP リクエストを受け付けられることだけを返します。
- controller は `--bind` の port（既定の例では 9000）、worker は `--bind` の port（既定の例では 9001）で待ち受けます。
- worker の `path_prefix` は `--public-url` のパスから決まります。リバースプロキシが prefix を付ける構成では、prefix 付きの health も利用できます。
- health は依存先（controller、storage、ffmpeg など）の readiness ではありません。worker の同期遅延や channel の停止は metrics のアラートで検知します。
- blackbox_exporter の `http_2xx` module が 200 を成功として扱います。自己署名証明書の worker を一時的に監視する場合だけ `http_2xx_insecure` を使います。

### Probe 対象の設定

Prometheus の `edamame-health` job は `prometheus/targets/edamame-health.yml` を file_sd で読みます。
blackbox_exporter から到達できる URL を設定してください。

```yaml
- targets: ["http://controller.example:9000/health"]
  labels:
    env: "prod"
    device: "edamame-controller"
- targets: ["http://worker.example:9001/health"]
  labels:
    env: "prod"
    device: "edamame-worker"
```

- `device` は Discord 通知とダッシュボードの表示名に使います。
- 自己署名証明書を使う場合だけ `probe_module: "http_2xx_insecure"` を追加します。通常は指定しません。
- `targets` が空の場合、health probe は実行されません。対象を追加するまで health アラートは no data のため発火しません。

## Grafana ダッシュボード

- フォルダ: `Edamame`
- UID: `edamame-overview`
- データソース: Prometheus `cfyat2m83vbpcd`
- 変数: `env`、`channel_id`、`quality`

パネルの構成:

- 概要: controller up、同期中の worker 数、ingest / publish レート、error レート
- Controller: worker の最終同期経過時間、weight、channel の最終受信・公開遅延・セグメント長、channel / quality ごとの流量、operation ごとのエラー
- Worker: worker up、controller への最終成功からの経過時間、cache の head sequence とセグメント数、download / serve レート、operation ごとのエラー
- Health: blackbox_exporter から見た controller / worker の `/health` 応答

## アラート

`Edamame` フォルダに以下のルールを provisioning しています。
通知先は既存の Discord contact point `infra 死活監視` です。

| UID | 条件 | 継続時間 | 重要度 |
| --- | --- | --- | --- |
| `edamame-collector-down` | Prometheus が OTel Collector を scrape できない | 2 分 | critical |
| `edamame-controller-down` | controller の稼働状態が停止、またはメトリクスを取得できない | 2 分 | critical |
| `edamame-worker-sync-stale` | controller から見て worker の最終同期が 60 秒以上前 | 3 分 | critical |
| `edamame-channel-ingest-stale` | channel の最終受信が、セグメント長の 5 倍 + 30 秒を超えている | 5 分 | warning |
| `edamame-published-lag` | channel の公開遅延が、セグメント長の 10 倍 + 30 秒を超えている | 5 分 | warning |
| `edamame-errors` | controller / worker のエラー回数が直近 5 分で増加している | 1 分 | warning |
| `edamame-health-probe` | blackbox_exporter から `/health` を取得できない | 2 分 | critical |

補足:

- `edamame-controller-down` 以外は、メトリクスが存在しない場合を `OK` として扱い、空のメトリクスで通知しない
- `edamame-health-probe` も、probe 対象が未設定でメトリクスが存在しない場合は `OK` として扱う
- worker のプロセス停止は、worker の `up` ではなく controller 側の `worker.last_sync_age` で検知する
- controller の停止は Collector が最後の値を保持するため、検知が遅れる場合がある
- health probe は controller / worker の HTTP 応答だけを確認する。worker の同期遅延や channel の停止は metrics のアラートで検知する
- 通知メッセージは `device` → `worker_id` → `channel_id` → `instance` → `job` の順で対象を表示する
- 意図した発火テストは未実施。通知が実際に Discord へ送られるため、実施する場合は事前に確認する

## ローカル確認用スタック

Grafana と OTLP の受け口（Tempo と Prometheus を含む）を 1 つのコンテナで動かす、開発用のスタックを用意している。
`grafana/otel-lgtm` を使う。

```bash
docker compose -f compose.observability.yaml up -d
```

公開する port:

| 用途 | アドレス |
| --- | --- |
| Grafana | http://127.0.0.1:3001 |
| OTLP (gRPC) | 127.0.0.1:4317 |
| OTLP (HTTP) | 127.0.0.1:4318 |

- ホストの `127.0.0.1` にだけ公開する
- Grafana はログイン不要
- 既存の Grafana と競合する場合は `GRAFANA_PORT` で変更する（既定は 3001）
- データは `lgtm-data` ボリュームに残る

起動の確認:

```bash
docker ps --filter name=edamame-lgtm
curl http://127.0.0.1:3001/api/health
```

停止と削除:

```bash
docker compose -f compose.observability.yaml stop   # 停止（データは残る）
docker compose -f compose.observability.yaml down   # コンテナとネットワークを削除
```

ローカルの Grafana は iromono の Grafana とは別物です。
`compose.observability.yaml` のスタックには `Edamame` ダッシュボードを provisioning していないため、ローカルでは Explore でメトリクスを確認します。

## アプリの送信先をローカルに向ける

アプリを起動する前に、送信先をローカルの OTLP 受け口に上書きする。
外部サービス向けの `OTEL_EXPORTER_OTLP_ENDPOINT` が既に設定されている環境では、上書きしないとローカルには届かない。

PowerShell:

```powershell
$env:OTEL_EXPORTER_OTLP_ENDPOINT = "http://127.0.0.1:4317"
$env:OTEL_METRIC_EXPORT_INTERVAL = "5000"
edamame-controller --bind 127.0.0.1:9000 --storage-capacity 512MiB --playlist-segments 6 --sync-interval-ms 1000 --sync-timeout-ms 10000
```

```powershell
$env:OTEL_EXPORTER_OTLP_ENDPOINT = "http://127.0.0.1:4317"
$env:OTEL_METRIC_EXPORT_INTERVAL = "5000"
edamame-worker --worker-id worker-a --bind 127.0.0.1:9001 --public-url http://127.0.0.1:9001 --controller-url http://127.0.0.1:9000 --cache-limit 12
```

bash:

```bash
OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4317 \
OTEL_METRIC_EXPORT_INTERVAL=5000 \
  edamame-controller ...
```

環境変数:

| 環境変数 | 説明 |
| --- | --- |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | 送信先。`http://iromono:4317` のように scheme と port まで含める。未指定または空のときは送信しない |
| `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` | metrics 専用の送信先。指定時は `OTEL_EXPORTER_OTLP_ENDPOINT` より優先される |
| `OTEL_SDK_DISABLED` | `true` を指定すると送信しない |
| `OTEL_METRIC_EXPORT_INTERVAL` | metrics の送信間隔（ミリ秒）。アプリの既定は 10000 |
| `OTEL_SERVICE_NAME` | `service.name` の上書き（省略時は `edamame-controller` / `edamame-worker` / `edamame-publisher`） |
| `OTEL_EXPORTER_OTLP_HEADERS` | 送信先が要求するヘッダー（`key=value` をカンマ区切り） |

`OTEL_EXPORTER_OTLP_ENDPOINT` に scheme がない場合は `http://` が補われる。

## 確認する

### Grafana

1. http://iromono:3000 を開く
2. `Edamame` フォルダの `Edamame` ダッシュボードを開く
3. `env`、`channel`、`quality` を必要に応じて絞り込む
4. controller / worker を起動し、セグメントを流して値が動くことを確認する

### Prometheus API で確認する

```bash
curl -s 'http://iromono:9090/api/v1/query?query=edamame_controller_up'
curl -s 'http://iromono:9090/api/v1/query?query=edamame_worker_up'
curl -s 'http://iromono:9090/api/v1/query?query=probe_success%7Bjob%3D%22edamame-health%22%7D'
```

health probe が設定されていれば、`probe_success` が 1 になります。`targets/edamame-health.yml` が空の場合は系列自体が存在しません。

メトリクス名の一覧:

```bash
curl -s 'http://iromono:9090/api/v1/label/__name__/values' | jq -r '.data[] | select(startswith("edamame_"))'
```

### アラートの確認

- Grafana の Alerting で `Edamame` フォルダのルールを確認する
- UID が `edamame-` で始まっていることを確認する
- 通知先が `infra 死活監視` であることを確認する
- 実際の発火テストは Discord に通知が飛ぶため、実施前に確認する

## データが届かないとき

- 送信先が正しいか。本番は `http://iromono:4317`、ローカルは `http://127.0.0.1:4317`
- アプリの標準エラーに `[telemetry]` の警告が出ていないか
- `OTEL_SDK_DISABLED=true` になっていないか
- metrics はアプリの既定で 10 秒間隔。確認中は `OTEL_METRIC_EXPORT_INTERVAL=5000` などで短くする
- Prometheus の `edamame` target が UP か確認する
- health probe を使う場合は、`edamame-health` target が UP か、`probe_success{job="edamame-health"}` が 1 か確認する
- blackbox_exporter から target の URL に到達できるか確認する。`targets/edamame-health.yml` の URL は blackbox_exporter のコンテナから解決できる必要がある
- 自己署名証明書の場合は `http_2xx_insecure` を明示する。通常の `http_2xx` では証明書エラーになる
- Collector の Prometheus exporter が `otel-collector:8889` で動いているか確認する
- Grafana の時間範囲を広げる
- collector の metric expiration は既定の 5 分。プロセス停止直後は最後の値が残ることがある
- コンテナは起動しているのに port に繋がらない場合は、Docker Desktop の port 公開が壊れていることがある。`docker desktop restart` で直ることがある
- port が競合していないか確認する（Grafana 3000、OTLP 4317 / 4318）

## Phase 2

Phase 2 では、まず publisher の OTLP metrics を導入し、その後 publisher を含む End-to-End の可観測性を追加します。
traces は要否が未定のため保留中です。

### 1. Publisher metrics

導入済み:

- `edamame-publisher` に `edamame-telemetry` を組み込み、controller / worker と同じ Collector へ OTLP metrics を送る
- 次のメトリクスを実装した
  - `edamame.publisher.up`
  - `edamame.publisher.segments.encoded`
  - `edamame.publisher.segments.published`
  - `edamame.publisher.upload.errors`
  - `edamame.publisher.queue.depth`
- publisher は同期スレッドで動くため、OTLP/tonic exporter 用の Tokio runtime を publisher 内で保持する
- Linux コンテナでコンパイル、ユニットテスト、iromono Collector への OTLP 送信を確認した

残り:

- channel / quality / input ごとに以下を追加する
  - 最終 publish 成功からの経過時間
  - encode 時間、encode エラー
  - ffmpeg プロセスの起動回数、異常終了、再接続
  - controller への upload 時間、再試行
  - 未送信セグメント数、drop したセグメント数
- publisher 用のダッシュボードとアラートを追加する
  - publisher down、publish stale、encode error rate、upload error rate、queue backlog
- Windows では publisher のビルド・実行が難しいため、Linux / WSL で検証する

### 2. Traces（保留）

trace の要否が未定のため、いったん保留にします。実装する場合の候補:

- OTel Collector に traces pipeline を追加し、Tempo へ送る
- Grafana に Tempo datasource を追加する
- controller / worker / publisher に span を追加する
  - controller: segment ingest、storage write、playlist publish、worker register / sync
  - worker: controller sync、segment download、cache put、client への segment 配信
  - publisher: encode、segment upload、playlist 更新
- HTTP リクエスト間で W3C Trace Context を伝播する
- サンプリング方針（通常時と障害時）を決める
- metrics から traces へ移動できるよう exemplar / trace ID を連携する
- 遅延・エラーを trace で調査できるダッシュボードを追加する

### 3. 運用と検証

- 実際の channel で End-to-End の負荷試験・長時間試験を行う
- Phase 1 のアラート閾値を実データで調整する
- publisher を含む通知の意図した発火テストを行う
- 障害時の調査手順を runbook として docs に追加する
- Phase 1 の health probe target を本番・検証環境の URL に更新する
