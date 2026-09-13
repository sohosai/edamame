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
ここで公開範囲とは、worker がキャッシュして sync で報告した範囲のうち、controller がプレイリストに反映すると決めた連続したセグメント番号の範囲を指す。
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
- `#EXT-X-MEDIA-SEQUENCE` は公開範囲の先頭のセグメント番号（HLS では media sequence number、メディアシーケンス番号）とし、client はこの値と `#EXTINF` の並びから、プレイリストを取得し直したときにセグメントの位置を合わせる
- `#EXTINF` は `duration_ms` を秒に直し、小数第 3 位まで固定で出力し、6000 ミリ秒は `6.000` になる
- セグメントはセグメント番号の昇順に出力する
- セグメントファイルの URL は `{public_url}/segments/{channel_id}/{quality}/{sequence}.ts` の絶対 URL とする
- すべての URL は、controller がその quality の配信先として選んだ worker の `public_url` を使う
- `public_url` は query と fragment を含まない URL とする
- Content-Type は `application/vnd.apple.mpegurl` とする

## マスタープレイリスト

マスタープレイリストは channel 内で公開範囲と配信先 worker がある quality を列挙する。
quality 間で公開範囲が違ってもよい。
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

controller は quality ごとに公開範囲を決める。
最初の未公開セグメントファイルを PUT した時刻から `--publish-wait-ms` だけ待つ。
追加の PUT では待ち時間を延長しない。
公開待ちの間は、未公開セグメントを公開範囲の候補にもフォールバックにも含めない。
公開待ちが終わった時点で存在するセグメントを公開可能として扱う。

公開範囲は、sync が `--sync-timeout-ms` 以内に届いている worker が連続してキャッシュしている公開可能範囲のうち、最新のものにする。
quality ごとに独立して決め、quality 間で範囲を一致させる必要はない。
接続性は保証せず、media sequence は前進・後退・飛び得る。
公開範囲は worker の sync を受けた時に更新し、プレイリストのリクエストでは変更しない。
現在の公開範囲を保持している worker がいれば、より新しい候補が出るまで維持する。
現在の公開範囲を保持している worker がいなければ、次の sync で、より古い範囲も含めて配信可能な最新の候補へ更新する。更新までのプレイリストリクエストは 503 になることがある。

worker が保持しているセグメントに欠落がある場合、controller は欠落を挟まない連続範囲だけを使う。
たとえば 101、102、104、105 を保持している場合、公開範囲の候補は 101..102 と 104..105 であり、最新側の 104..105 を優先する。

公開範囲のファイルが欠落している場合、controller はその quality のメディアプレイリストを生成せず、422 を返す。

同じ channel の全セグメントの `duration_ms` は同じ値で、500 以上でなければならない。
`--playlist-segments` は 6 以上とするが、worker が保持している連続範囲が短い場合は、その短い範囲を公開することがある。

マスタープレイリストは、公開範囲と配信先 worker がある quality を列挙する。
公開範囲がまだない quality と、配信先 worker がない quality は一時的に含めない。
配信できる quality が 1 つもない場合は 503 を返す。

プレイリストはリクエストごとに現在の公開範囲から生成する。
controller は生成済みのプレイリストを保存しない。
公開範囲が変わっていない間は、同じ公開範囲のプレイリストを返す。

ステータスコードは 400、404、422、503 の順に判定する。
公開範囲またはその quality の配信先 worker がない場合は 503 を返し、controller のセグメントファイルの URL には逃がさない。
