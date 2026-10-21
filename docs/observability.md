# 可観測性

controller と worker の OpenTelemetry 計装は現在未実装。
この文書は、計装を入れたあとにローカルで traces と metrics を確認するための手順をまとめる。
環境変数、span、metrics の仕様は、実装とあわせてこの文書へ追記する。

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

確認用の環境変数（実装とあわせて確定する）:

| 環境変数 | 説明 |
| --- | --- |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | 送信先。`http://127.0.0.1:4317` のように scheme と port まで含める。未指定または空のときは送信しない |
| `OTEL_SDK_DISABLED` | `true` を指定すると送信しない |
| `OTEL_METRIC_EXPORT_INTERVAL` | metrics の送信間隔（ミリ秒）。既定は 60000。確認中は 5000 などに短くする |
| `OTEL_SERVICE_NAME` | `service.name` の上書き（省略時は実装側の既定値） |
| `OTEL_EXPORTER_OTLP_HEADERS` | 送信先が要求するヘッダー（`key=value` をカンマ区切り） |

## 確認する

### Grafana

1. http://127.0.0.1:3001 を開く
2. 左メニューの Explore を開く
3. データソースに Tempo を選び、`service.name` で controller または worker を検索する
4. データソースに Prometheus を選び、`edamame_` で始まる metrics を検索する

### API で確認する

自動確認やスクリプトからは Grafana の API を使う。
otel-lgtm ではデータソースの uid は `prometheus` と `tempo`。

データソース一覧:

```bash
curl -s http://127.0.0.1:3001/api/datasources
```

Prometheus の metrics 名の一覧:

```bash
curl -s http://127.0.0.1:3001/api/datasources/proxy/uid/prometheus/api/v1/label/__name__/values
```

Prometheus の値:

```bash
curl -s -X POST --data-urlencode "query=<metric_name>" \
  http://127.0.0.1:3001/api/datasources/proxy/uid/prometheus/api/v1/query
```

Tempo の trace 検索:

```bash
curl -s "http://127.0.0.1:3001/api/datasources/proxy/uid/tempo/api/search?tags=service.name%3D<service_name>&limit=5"
```

trace の詳細:

```bash
curl -s -H "Accept: application/json" \
  http://127.0.0.1:3001/api/datasources/proxy/uid/tempo/api/traces/<trace_id>
```

`<metric_name>` などの具体的な名前は、計装の実装時に決めてこの文書へ追記する。

## データが届かないとき

- 送信先がローカルになっているか。上書きを忘れると外部サービスへ送られる
- アプリの標準エラーに、OTLP の初期化失敗の警告が出ていないか
- metrics は既定で 60 秒間隔。確認中は `OTEL_METRIC_EXPORT_INTERVAL=5000` などで短くする
- Grafana の Tempo / Prometheus で時間範囲を広げる
- コンテナは起動しているのに port に繋がらない場合は、Docker Desktop の port 公開が壊れていることがある。`docker desktop restart` で直ることがある
- port が競合していないか確認する（Grafana 3000、OTLP 4317 / 4318）

## 実装時に追記する内容

- 環境変数の一覧と既定値
- traces（span 名と属性）の一覧
- metrics（名前、種類、ラベル）の一覧
- PromQL の例
- 送信先の指定がないとき、または OTLP を初期化できないときにアプリを止めない方針
- 計装が既存のログ出力を変えない方針
