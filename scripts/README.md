# スクリプト

## `loop-publisher.py`

`scripts/loop-publisher.py` は、mp4 ファイルをループ再生し、controller へ PUT し続けるテスト用の publisher である。
再エンコードは行わず、素材のキーフレームの位置でセグメントを切る。

### 必要な環境

- Python 3
- ffmpeg と ffprobe
- 起動済みの controller と worker

### 素材の準備

素材には、セグメント長の倍数の位置にキーフレームが必要であり、キーフレームの間隔も揃っていなければならない。
素材の長さもセグメント長の倍数にする。フレームの量子化により、キーフレームと素材の長さは 1 フレーム以内のずれを許容する。
この許容の分だけ、セグメントの長さは指定した値と 1 フレーム以内でずれ、ループの継ぎ目でも長さが変わる。
起動時に ffprobe で確認し、合わない場合はエラーを出して終了する。

音声を持つ素材では、AAC のエンコーダの遅延（プリミング）が先頭のパケットの負の PTS として残る。
この分だけ 1 周のタイムスタンプの進み方が素材の長さより大きくなるため、ループの継ぎ目をまたぐ
セグメントは、素材の長さの端数とプリミング（48kHz で約 21ms）を足した分だけ長くなる。

6 秒のセグメントを使う 720p の素材は、次のコマンドで作る。

```bash
ffmpeg -y -i input.mp4 \
  -map 0:v:0 -map 0:a:0 \
  -vf scale=1280:720 -r 30 \
  -c:v libx264 -preset ultrafast -tune zerolatency -pix_fmt yuv420p \
  -b:v 3000000 -maxrate 3000000 -bufsize 6000000 \
  -force_key_frames 'expr:gte(t,n_forced*6)' -g 180 -sc_threshold 0 \
  -c:a aac -b:a 128k -ar 48000 -ac 2 \
  -t 630 \
  source-720p.mp4
```

`-t` にはセグメント長の倍数を指定する。上の例は 6 秒の倍数である 630 秒である。
入力が 630 秒より短い場合は、入力の長さ以下で最大の 6 の倍数を指定する。

### 使い方

```bash
python3 scripts/loop-publisher.py \
  --source ./source-720p.mp4 \
  --controller-url http://127.0.0.1:9000 \
  --channel-id live \
  --quality 720p \
  --bandwidth 3000000 \
  --segment-duration-ms 6000 \
  --resolution 1280x720 \
  --auth-token example-token
```

セグメントの識別子はセグメントごとにランダムに生成する。
番号は controller が割り当てる。
publisher は割り当てられた `sequence` を標準出力へ記録する。

### 再起動

publisher を停止して再起動すると、master playlist から channel 全体の次の `sequence` を求め、
その番号のセグメントが始まる時刻を `-output_ts_offset` に渡す。
番号は続きから進み、再起動後の最初のセグメントの映像は続きの時刻から始まる
（音声は AAC のプリミングの分だけ前になることがある）。
素材の内容は先頭に戻る。

ffmpeg は実行ごとに素材の先頭から読み、セグメントの区切りもその実行の先頭から数え直す。
このため、再起動した実行のセグメントの長さと切り位置は、素材の長さの端数、プリミング、
キーフレームの量子化によって、指定した値や停止せずに動かし続けた場合とずれることがある。
ずれの方向は前後どちらにもなり、ずれた分だけ直前のセグメントと時刻が重なったり、
間が空いたりすることがある。
ずれは再起動のたびに積み上がらない。

### オプション

| option | 必須 | 説明 |
| --- | --- | --- |
| `--source` | 必須 | 入力 mp4 のパス。キーフレームの位置と長さをセグメント長に合わせる |
| `--controller-url` | 必須 | controller に接続するための絶対 http URL |
| `--channel-id` | 必須 | channel の識別子 |
| `--quality` | 必須 | quality の識別子 |
| `--bandwidth` | 必須 | controller に宣言する `bandwidth`（bit/s）。素材の実ビットレートに合わせる |
| `--segment-duration-ms` | 必須 | セグメント長（500 以上の整数、ミリ秒）。素材の 1 フレーム以上にし、キーフレーム間隔と長さに合わせる |
| `--resolution` | 任意 | controller に宣言する解像度（`WIDTHxHEIGHT`）。素材の解像度に合わせる |
| `--auth-token` | 任意 | controller の `--auth-token` と同じトークン（空白と制御文字を除く表示可能 ASCII、省略すると `Authorization` ヘッダーを送らない） |
| `--external-m3u8` | 任意 | channel の外部配信の master m3u8 の URL。指定すると、最初のセグメントを公開した後に controller へ登録する |

### 注意

- mp4 は先頭から無限に繰り返す
- 素材の先頭にはキーフレームが必要である
- 起動時にキーフレームの位置と間隔、素材の長さを確認し、セグメント長に合わない場合は終了する
- セグメントの長さは、キーフレームの量子化の分だけ指定した値とずれることがある。ループの継ぎ目では、素材の長さの端数やプリミングの影響も加わり、セグメントの長さが変わることがある
- 再起動した実行では、セグメントの長さと切り位置が連続実行とずれることがある（再起動の節を参照）
- セグメントの識別子は毎回ランダムに生成する
- master playlist または media playlist を取得できない場合は、ffmpeg を起動する前に終了する
