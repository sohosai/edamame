# スクリプト

## `loop-publisher.py`

`scripts/loop-publisher.py` は、mp4 ファイルをループ再生し、controller へ PUT し続けるテスト用の publisher である。

### 必要な環境

- Python 3
- libx264 と AAC を含む ffmpeg と ffprobe
- 起動済みの controller と worker

### 使い方

```bash
python3 scripts/loop-publisher.py \
  --source ./sample.mp4 \
  --controller-url http://127.0.0.1:9000 \
  --channel-id live \
  --quality 720p \
  --bandwidth 3000000 \
  --segment-duration-ms 6000 \
  --resolution 1280x720
```

セグメントの識別子はセグメントごとにランダムに生成する。
番号は controller が割り当てる。
publisher は割り当てられた `sequence` を標準出力へ記録する。

### オプション

| option | 必須 | 説明 |
| --- | --- | --- |
| `--source` | 必須 | 入力 mp4 のパス |
| `--controller-url` | 必須 | controller に接続するための絶対 http URL |
| `--channel-id` | 必須 | channel の識別子 |
| `--quality` | 必須 | quality の識別子 |
| `--bandwidth` | 必須 | x264 の目標ビットレートと controller に宣言する `bandwidth`（bit/s） |
| `--segment-duration-ms` | 必須 | セグメント長（500 以上の整数、ミリ秒） |
| `--resolution` | 任意 | controller に宣言する解像度（`WIDTHxHEIGHT`）。映像は変換しない |

### 注意

- mp4 は先頭から無限に繰り返す
- セグメント長は入力映像の 1 フレーム以上にする
- セグメントの識別子は毎回ランダムに生成する
- master playlist または media playlist を取得できない場合は、ffmpeg を起動する前に終了する
- PUT に失敗した場合は作業ディレクトリを残し、ファイルのパスと再送用の curl コマンドを出力する
