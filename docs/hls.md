# HLS の出力

Edamame は publisher から受け取ったセグメントの属性からマスタープレイリストとメディアプレイリストを生成する。
この文書は client に渡すプレイリストの形式を定める。

## 対応する形式

- セグメントファイルは MPEG-TS (`.ts`) だけを扱う
- `#EXT-X-VERSION` は 3 とする
- ライブ配信だけを扱い、`#EXT-X-ENDLIST` は出力しない
- fMP4、LL-HLS、バイト範囲、暗号化、複数音声、I フレーム用のプレイリストは扱わない

## メディアプレイリスト

メディアプレイリストは 1 つの quality のセグメントを列挙する。
ここで配信範囲とは、存在する最も古いセグメント番号から最新のセグメント番号までの範囲を指す。
たとえば `live` channel の `720p` quality にセグメント番号 101 と 102 があり、どちらも `duration_ms` が 6000、controller が `worker-a` を選んだ場合、出力は次のようになる。

```text
#EXTM3U
#EXT-X-VERSION:3
#EXT-X-TARGETDURATION:6
#EXT-X-MEDIA-SEQUENCE:101
#EXTINF:6.000,
http://127.0.0.1:9001/segments/live/720p/101.ts
#EXTINF:6.000,
http://127.0.0.1:9001/segments/live/720p/102.ts
```

各要素の決まりは次のとおり。

- `#EXT-X-TARGETDURATION` には channel のセグメント長の基準値（秒）を出力し、値は channel の最初に成功した PUT の `duration_ms` を秒に直し、小数点以下を切り上げた整数（最低 1）として決まり、channel が存在する間は変わらない
- `#EXT-X-MEDIA-SEQUENCE` は配信範囲の先頭のセグメント番号（HLS では media sequence number、メディアシーケンス番号）とし、client はこの値と `#EXTINF` の並びから、プレイリストを取得し直したときにセグメントの位置を合わせる
- `#EXTINF` は `duration_ms` を秒に直し、小数第 3 位まで固定で出力し、6000 ミリ秒は `6.000` になる
- セグメントはセグメント番号の昇順に出力する
- セグメントファイルの URL は `{public_url}/segments/{channel_id}/{quality}/{sequence}.ts` の絶対 URL とする
- すべての URL は、controller が選んだ 1 つの `READY` の worker の `public_url` を使う
- `public_url` は query と fragment を含まない URL とする
- Content-Type は `application/vnd.apple.mpegurl` とする

## マスタープレイリスト

マスタープレイリストは channel 内の quality を列挙する。
quality は昇順に出力する。
`1080p` と `720p` がある場合、出力は次のようになる。

```text
#EXTM3U
#EXT-X-VERSION:3
#EXT-X-STREAM-INF:BANDWIDTH=6000000,RESOLUTION=1920x1080
1080p.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=3000000,RESOLUTION=1280x720
720p.m3u8
```

各要素の決まりは次のとおり。

- `#EXT-X-STREAM-INF` の `BANDWIDTH` は必須とし、quality の `bandwidth` をそのまま使う
- `RESOLUTION` は `resolution` がある場合だけ出力する
- メディアプレイリストへの URL は `{quality}.m3u8` の相対 URL とし、client は controller からメディアプレイリストを取得する
- Content-Type は `application/vnd.apple.mpegurl` とする

## プレイリストを生成できる条件

controller はメディアプレイリストを quality ごとに生成する。
直近のセグメントの範囲で、存在する最も古いセグメント番号から最新のセグメント番号までに欠落したセグメント番号がある場合、controller はそのメディアプレイリストを生成せず、422 を返す。
欠落したセグメントを除いてプレイリストを生成すると client が前後のセグメントを連続して再生できないため、controller は欠落が埋まるまでプレイリストを生成しない。
たとえばセグメント番号 101、102、104、105 があり、103 がない場合、controller は 103 を含まないメディアプレイリストを生成できない。
103 が PUT されると、次のリクエストからメディアプレイリストを生成できる。

controller は直近のセグメントの範囲の先頭より古いセグメントファイルがない場合、存在する最も古いセグメント番号から最新のセグメント番号までを出力する。
開始直後の短いメディアプレイリストはこの条件で生成する。

同じ channel の全セグメントの `duration_ms` は同じ値で、500 以上でなければならない。
controller は `--playlist-segments` を 6 以上に制限する。
これにより、直近のセグメントがそろった後に古いセグメントファイルを削除しても、配信範囲の合計時間がセグメント長の基準値の 3 倍以上になる。

マスタープレイリストは、channel 内の全 quality の配信範囲が一致する場合だけ生成する。
一致しない場合は 422 を返す。

プレイリストはリクエストごとに現在のメタデータから生成する。
controller は生成済みのプレイリストを保存せず、生成に失敗した場合に古いプレイリストを返さない。

ステータスコードは 400、404、422、503 の順に判定する。
`READY` の worker がない場合は 503 を返し、controller のセグメントファイルの URL には逃がさない。
