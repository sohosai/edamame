# HLS の出力

Edamame は publisher から受け取った segment の属性から master playlist と media playlist を生成する。
この文書は client が依存する出力の契約を定める。

## 対応する形式

- segment は MPEG-TS (`.ts`) だけを扱う。
- `#EXT-X-VERSION` は 3 とする。
- live 配信だけを扱い、`#EXT-X-ENDLIST` は出力しない。
- fMP4、LL-HLS、byte-range、暗号化、複数音声、I-frame playlist は扱わない。

## media playlist

media playlist は 1 つの quality の segment を列挙する。
ここで配信範囲とは、存在する最も古い sequence から live edge までの範囲を指す。
たとえば `live` channel の `720p` quality に sequence 101 と 102 があり、どちらも `duration_ms` が 6000、controller が `cache-a` を選んだ場合、出力は次のようになる。

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

各要素の契約は次のとおり。

- `#EXT-X-TARGETDURATION` は channel の target duration とする。値は channel の最初に成功した PUT の `duration_ms` を秒に直し、小数点以下を切り上げた整数（最低 1）として決まり、channel が存在する間は変わらない。
- `#EXT-X-MEDIA-SEQUENCE` は配信範囲の先頭の sequence とする。
- `#EXTINF` は `duration_ms` を秒に直し、小数第 3 位まで固定で出力する。6000 ミリ秒は `6.000` になる。
- segment は sequence の昇順に出力する。
- segment の URL は `{public_url}/segments/{channel_id}/{quality}/{sequence}.ts` の絶対 URL とする。
- すべての URL は、controller が選んだ 1 つの `READY` の cache の `public_url` を使う。
- `public_url` は query と fragment を含まない URL とする。
- Content-Type は `application/vnd.apple.mpegurl` とする。

## master playlist

master playlist は channel 内の quality を列挙する。
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

各要素の契約は次のとおり。

- `#EXT-X-STREAM-INF` の `BANDWIDTH` は必須とし、quality の `bandwidth` をそのまま使う。
- `RESOLUTION` は `resolution` がある場合だけ出力する。
- media playlist への URL は `{quality}.m3u8` の相対 URL とする。client は controller から media playlist を取得する。
- Content-Type は `application/vnd.apple.mpegurl` とする。

## playlist を生成できる条件

media playlist は quality ごとに生成する。
live edge までの間に欠落した sequence がある場合は生成せず、422 を返す。

window の先頭より古い segment がない場合は、存在する最も古い sequence から live edge までを出力する。
開始直後の短い media playlist はこの条件で生成する。

同じ channel の全 segment の `duration_ms` は同じ値で、500 以上でなければならない。
controller は `--live-window` を 6 以上に制限する。
これにより、window が埋まった後に古い segment を削除しても、配信範囲の合計 duration が target duration の 3 倍以上になる。

master playlist は、channel 内の全 quality の配信範囲が一致する場合だけ生成する。
一致しない場合は 422 を返す。

playlist はリクエストごとに現在のメタデータから生成する。
controller は生成済みの playlist を保存せず、生成に失敗した場合に古い playlist を返さない。

ステータスコードは 400、404、422、503 の順に判定する。
`READY` の cache がない場合は 503 を返し、controller の segment URL には逃がさない。
