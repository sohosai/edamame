# tomyou

`tomyou` は、拠点の Windows、Linux、macOS で動く配信アプリである。
OBS の映像を Edamame と ImageFlux へ同時に送り、送信先ごとの状態を GUI に表示する。

この文書では、構成、OBS と ffmpeg の条件、ImageFlux の扱い、障害時の動作、検証方法を説明する。
ビルドと起動の最短手順は [`crates/tomyou/README.md`](../crates/tomyou/README.md) を参照する。

## 構成

```text
OBS ── RTMP / H.264 + AAC ──> tomyou の RTMP サーバ
                               ├─ ffmpeg ──> edamame-publisher
                               │                ├─> controller / worker
                               │                └─> live2025-server に既存方式で登録
                               └─ ffmpeg ──> WHIP ──> ImageFlux
```

`tomyou` は OBS の配信先をローカルの RTMP サーバへ変更する。
RTMP サーバは 1 接続の publish だけを受け付け、複数のローカル play 接続へフレームを分配する。
Edamame と ImageFlux への送信は、それぞれ独立した ffmpeg プロセスが行う。

- Edamame 向けは H.264 と AAC をコピーする。
- ImageFlux 向けは映像をコピーし、音声だけ Opus の 48 kHz ステレオへ変換する。
- ImageFlux のチャンネル作成、WHIP URL の生成、プレイリスト URL の取得、削除は `whip-script` の API 操作を引き継ぐ。
- ImageFlux のプレイリストと鍵を live2025-server へ登録する処理は行わない。
- Edamame の URL と鍵の登録は、既存の `edamame-publisher` が行う。

## OBS の条件

OBS は 30.2 以降の WebSocket 5.5 を使う。
 `tomyou` は OBS WebSocket で次の操作を行う。

1. 既に配信中でないことを確認する。
2. 変更前の配信先を保存する。
3. 配信先を `rtmp_custom` の `rtmp://127.0.0.1:1935/live/<channel>` にする。
4. 配信を開始する。
5. 停止時に、配信先がまだ `tomyou` の設定なら元へ戻す。

OBS の入力は H.264、B フレームなし、AAC 音声を前提とする。
キーフレーム間隔は publisher のセグメント長を割り切る値にする。
6 秒セグメントでは 2 秒を推奨する。
OBS のシーン、ソース、解像度を `tomyou` が自動変更することはない。

OBS が既に配信中の場合は、`tomyou` は開始しない。
OBS WebSocket だけが切れても、RTMP 入力が続いている間は送信を維持する。

## ffmpeg の条件

ffmpeg は次の機能を持つ必要がある。

- WHIP muxer
- DTLS
- libopus encoder
- RTMP protocol
- FLV demuxer
- AAC decoder

起動時に実際に ffmpeg を動かして確認する。
不足している場合は不足機能を画面に表示する。
ImageFlux の機能が足りない場合も、Edamame 側の送信は開始する。

Windows の配布ビルドでは ffmpeg を実行ファイルへ埋め込む。
Linux と macOS では外部の ffmpeg を使い、PATH、`TOMYOU_FFMPEG`、標準的なインストール先から探す。
`ffmpeg.path` が設定されていれば、それを最優先する。

WHIP の実装は、映像が H.264 で B フレームを含まないこと、音声が 48 kHz ステレオであることを要求する。
音声は AAC から Opus へ変換する。
映像の縮小や再エンコードへ自動で切り替えない。

## ImageFlux

ImageFlux には `CreateMultistreamChannelWithHLS` でチャンネルを作る。
返された Sora URL とチャンネル ID から `https://<host>/whip/<channel_id>` を作る。
HLS 設定は `whip-script` の設定を取り込み、解像度、FPS、ビットレート、セグメント長、アーカイブ先を保持する。

プレイリスト URL は `ListPlaylistURLs` で取得し、画面に表示する。
30 秒以内に生成されない場合は「生成待ち」として表示する。
チャンネル ID は状態ファイルへ保存する。
停止時に削除できなかった ID は残し、次回起動時に削除を再試行する。

この版では、ImageFlux のプレイリスト URL と鍵を live2025-server へ登録しない。
また、ImageFlux の鍵取得、鍵の保存、鍵配布も行わない。
既存の `encrypt_key_uri` は保持するが、Edamame の鍵だけを返す現在のエンドポイントを ImageFlux の鍵配布先として流用できるとは扱わない。

したがって、この版だけで視聴 client の自動フォールバックを完成させない。
画面に HLS URL を表示できても、復号再生を確認できるまでは「視聴可能」と表示しない。

## 障害時の動作

| 障害 | 動作 |
| --- | --- |
| OBS に接続できない | 配信を開始しない |
| OBS が既に配信中 | 既存の配信を停止せず、開始を拒否する |
| ImageFlux のチャンネル作成に失敗 | Edamame だけで配信を開始する |
| ImageFlux の WHIP が切れる | ImageFlux の ffmpeg だけをバックオフして再起動する。Edamame は継続する |
| ImageFlux の HLS が生成されない | Edamame は継続し、画面に生成待ちを表示する |
| edamame-publisher が停止、または RTMP ポートへ到達できない | ImageFlux は継続する。Edamame 側の ffmpeg だけを再起動する。GUI のログに `Connection refused` などを表示する |
| publisher の URL のチャンネル名が違う | publisher は `1A` など大文字小文字を区別する。`rtmp://albert:1936/live/1A` のように設定する |
| RTMP 入力が切れる | 出力を止め、OBS の再接続を待つ |
| ffmpeg が起動できない | 原因を画面に表示し、バックオフして再試行する |

ImageFlux と Edamame の出力は別プロセスなので、片方の停止が他方へ直接波及しない。
ただし、OBS、`tomyou`、拠点 PC、拠点回線は両系で共有する。
これらが停止すれば両系が止まる。
上り帯域は原則として映像約 2 本分必要になる。

## 設定と秘密情報

設定は OS 標準のユーザーディレクトリへ保存する。

- Windows: `%APPDATA%\tomyou\config.toml`
- macOS: `~/Library/Application Support/tomyou/config.toml`
- Linux: `~/.config/tomyou/config.toml`

起動時に `--config <FILE>` を指定すると、そのファイルを読み込み、保存先にも使う。
GUI の「設定」にある「設定ファイル」でも、任意のパスを指定して読み込み・保存できる。
指定したファイルを読めない場合も GUI は初期値で起動し、設定画面でパスを直せる。

OBS のパスワードと ImageFlux の API トークンも設定ファイルへ保存する。
Unix では保存時に 0600 へ変更し、所有者だけが読めるようにする。
設定画面の「OS のアプリで開く」から、OS の既定アプリで TOML を直接編集できる。
ファイルが無い場合は、現在の設定を保存してから開く。
編集後は「再読込」を押す。
Windows では設定ファイルの ACL を別途制限する。
秘密情報を含むため、設定ファイルを配布したりリポジトリへ commit したりしない。
ログと画面のコピー機能には秘密情報を出さない。

## 配布

Windows では `tomyou.exe` に ffmpeg を埋め込む。
`TOMYOU_FFMPEG_PATH` に固定した WHIP 対応の `ffmpeg.exe` を指定し、`bundled-ffmpeg` を有効にしてビルドする。
実行時はユーザー領域へ展開し、ハッシュを確認する。

Linux と macOS では ffmpeg を埋め込まない。
利用者または管理者が用意した ffmpeg を検査して使う。

macOS は Apple Silicon と Intel でビルドを分ける。
署名と notarization は資格情報がある場合に行い、未署名の場合はその旨を明記する。
Gatekeeper を自動で無効化しない。

ffmpeg のライセンスは実際のビルド内容で決まる。
`--enable-nonfree` を使わず、配布物に対応ソース、ビルド設定、ライセンスを添える。

## 検証

単体テストは設定、URL、ImageFlux の JSON、ffmpeg の引数を確認する。
実サービスを使う確認は次の順で行う。

1. 実 OBS から `tomyou` の RTMP サーバへ publish する。
2. 実 publisher へ RTMP を送り、Edamame の全画質とプレイリストを確認する。
3. 実 ImageFlux の WHIP へ送り、HLS の解像度と音声を確認する。
4. Edamame 側の ffmpeg を止めても ImageFlux が継続することを確認する。
5. ImageFlux 側の ffmpeg を止めても Edamame が継続することを確認する。
6. OBS の停止、再開、`tomyou` の停止で子プロセスが残らないことを確認する。

ImageFlux の復号再生は、既存の鍵配布手段が使える環境で確認する。
鍵配布が未接続の場合は、その部分を未検証として記録する。

ローカル RTMP の遅延は、初回のキーフレーム待ち、再接続、HLS の遅延と分けて測る。
定常時の中継追加遅延は 100 ms 未満を目標とする。

## 制約

- 録画、映像プレビュー、遠隔操作、複数チャンネル同時配信は行わない。
- 映像の自動再エンコード、自前 WebRTC、ImageFlux の RTMP ingest は行わない。
- `tomyou` と `whip-script` を同じ OBS に対して同時に動かさない。
- Edamame と ImageFlux の独立した冗長化ではない。
