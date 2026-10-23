# HLS の出力

この文書では、`controller` が `client` に返すマスタープレイリストとメディアプレイリストの出力形式、および生成条件を説明する。
全体構成と用語の定義は [docs/README.md](./README.md) で説明する。

Edamame では、`publisher` はセグメントファイル（`.ts`）だけを `controller` に `PUT` し、プレイリスト（`.m3u8`）は送信しない。
`controller` は、受け取ったセグメントの属性と `worker` のキャッシュ状況（公開範囲）を基に、リクエストのたびにメモリ上でプレイリストを生成して返す。

## 対応する形式

- セグメントファイルは MPEG-TS（`.ts`）だけを扱う
- `#EXT-X-VERSION` は `3` とする
- ライブ配信だけを扱い、`#EXT-X-ENDLIST` は出力しない
- fMP4、LL-HLS、バイト範囲、複数音声、I フレーム用のプレイリストは扱わない
- 暗号化は AES-128（CBC モード、PKCS7 パディング）だけを扱う。セグメント本体の暗号化と暗号鍵の配信は `publisher` 側が行い、`controller` はセグメントの `PUT` 時に渡された `key_uri` と `iv` を `#EXT-X-KEY` タグとしてメディアプレイリストに出力する

## メディアプレイリスト

### 出力例とタグの決まり

メディアプレイリスト（`GET /channels/{channel_id}/{quality}.m3u8`）は、1 つの `quality` の公開範囲に含まれるセグメントを列挙する。
たとえば `live` channel の `720p` quality にセグメント番号 101 と 102 があり、どちらも `duration_ms` が 6000 で、`controller` が配信先として `worker-a`（公開 URL が `http://127.0.0.1:9001`）を選んだ場合、出力は次のようになる。

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

- `#EXT-X-TARGETDURATION` には `channel` のセグメント長の基準値（秒）を出力する。基準値は、その `channel` で最初に成功した `PUT` の `duration_ms` を秒に直し、小数点以下を切り上げた整数（下限は 1）とする。`channel` が存在する間は変わらない
- `#EXT-X-MEDIA-SEQUENCE` には公開範囲の先頭のセグメント番号（HLS のメディアシーケンス番号）を出力する。`client` はプレイリストを取得し直したときに、この値と `#EXTINF` の並びからセグメントの再生位置を合わせる
- `#EXTINF` には `duration_ms` を秒に直し、小数第 3 位まで固定で出力する（6000 ミリ秒は `6.000` になる）
- セグメントはセグメント番号の昇順に出力する
- セグメントファイルの URL は `{public_url}/segments/{channel_id}/{quality}/{sequence}.ts` の絶対 URL とする
- 1 回のメディアプレイリスト応答に含まれるすべてのセグメント URL には、`controller` がそのリクエスト時点で選んだ 1 台の `worker` の `public_url` を使う（複数の稼働 `worker` がある場合は重みの比に従ってリクエストごとに選び直す）
- `public_url` は query と fragment を含まない URL とする
- Content-Type は `application/vnd.apple.mpegurl` とする

### 暗号化されたセグメント

`key_uri` を付けて `PUT` されたセグメントには `#EXT-X-KEY` を出力する。
暗号鍵の URI には `publisher` が指定した絶対 URL を使う。
`controller` と `worker` は暗号鍵そのものを取得も検証もせず、暗号化されたセグメントファイルをそのまま保存および配信する。

たとえばセグメント番号 101 と 102 が `https://keys.example.test/live/key-1` と IV `0x00000000000000000000000000000000`、103 が `https://keys.example.test/live/key-2` と IV `0x00000000000000000000000000000001`、104 が暗号化なしで、公開範囲が 101 から 104 の場合、出力は次のようになる。

```text
#EXTM3U
#EXT-X-VERSION:3
#EXT-X-TARGETDURATION:6
#EXT-X-MEDIA-SEQUENCE:101
#EXT-X-KEY:METHOD=AES-128,URI="https://keys.example.test/live/key-1",IV=0x00000000000000000000000000000000
#EXTINF:6.000,
http://127.0.0.1:9001/segments/live/720p/101.ts
#EXT-X-KEY:METHOD=AES-128,URI="https://keys.example.test/live/key-1",IV=0x00000000000000000000000000000000
#EXTINF:6.000,
http://127.0.0.1:9001/segments/live/720p/102.ts
#EXT-X-KEY:METHOD=AES-128,URI="https://keys.example.test/live/key-2",IV=0x00000000000000000000000000000001
#EXTINF:6.000,
http://127.0.0.1:9001/segments/live/720p/103.ts
#EXT-X-KEY:METHOD=NONE
#EXTINF:6.000,
http://127.0.0.1:9001/segments/live/720p/104.ts
```

各要素の決まりは次のとおり。

- 暗号鍵の URI があるセグメントの直前に `#EXT-X-KEY:METHOD=AES-128,URI="{key_uri}"` を出力する。IV がある場合は `,IV=0x{iv}` を続ける。IV が省略されたセグメントでは `IV` 属性を出力せず、`client` は HLS 仕様（RFC 8216）に従いメディアシーケンス番号を IV として復号する
- 暗号鍵の URI と IV が直前のセグメントと同じであっても、暗号化されたセグメントには毎回 `#EXT-X-KEY` タグを出力する
- 暗号化されたセグメントの後に暗号化なしのセグメントが続く箇所では、以降の復号を止めるために `#EXT-X-KEY:METHOD=NONE` を 1 回出力する
- 暗号鍵の URI を 1 つも受け取っていない `quality` には `#EXT-X-KEY` を出力しない
- 暗号鍵の URI と IV は `controller` のメモリ上だけに保持し、`controller` の再起動で失われる

## マスタープレイリスト

### 出力例とタグの決まり

マスタープレイリスト（`GET /channels/{channel_id}/master.m3u8`）は、`channel` 内で公開範囲と配信先 `worker` がそろっている `quality` を列挙する。
`quality` 間で公開範囲が異なっていてもよく、`quality` は名前の辞書順（昇順）に出力する。
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

- `#EXT-X-STREAM-INF` の `BANDWIDTH` は必須とし、`quality` の `bandwidth` をそのまま使う
- `RESOLUTION` は `resolution` が設定されている場合だけ出力する
- メディアプレイリストへの URL は `{quality}.m3u8` の相対 URL とし、`client` は `controller` からメディアプレイリストを取得する
- Content-Type は `application/vnd.apple.mpegurl` とし、`200 OK` と `302 Found` のレスポンスには `Cache-Control: no-store` を付ける

### 外部配信への振り分け

`channel` に外部配信のマスタープレイリスト URL が登録されているとき、`master.m3u8` はリクエストごとの判定で `302 Found` を返し、`Location` にその URL を返すことがある。
判定には、接続元の IP と `channel` から決まる値と、その時点の外部配信へ振り分ける割合を使う。

外部配信へ振り分ける割合は、配信できる `quality` の公開範囲が最新セグメントより遅れるほど高くなる。
遅れが `--publish-wait-ms` 相当のセグメント数以内であれば、`--external-min-percent`（既定値 1%）になる。
そこから遅れが増えるにつれて割合を線形に引き上げ、遅れが `--playlist-segments` 個に達した時点で 100% にする。
配信できる `quality` が 1 つもないときは、すべてのリクエストを外部配信へ振り分ける。

`controller` は外部のプレイリストを取得せず、振り分け先が使えるかどうかも確認しない。
外部配信を停止するときは、`PUT /channels/{channel_id}/external` に `null` を送って登録を取り消す。

`master.m3u8?external=0` のように `external=0` を付けたリクエストには振り分けを適用しない。
`external=1` を付けると割合に関係なく外部配信へ振り分けるが、外部配信が登録されていなければ自前のマスタープレイリストを返す。
`external` に `0` と `1` 以外の値を指定した場合は `400 Bad Request` を返す。

## 生成条件とステータスコード

### 公開範囲と連続範囲の扱い

`controller` が `quality` ごとに公開範囲を決める仕組みは [docs/README.md](./README.md) の「2. worker の同期と公開範囲の決定」に従う。
プレイリストの生成に関しては、次の規則が適用される。

- `worker` が保持しているセグメントに欠番がある場合、`controller` は欠番を挟まない連続範囲だけを公開範囲の候補にする。たとえば 101、102、104、105 を保持している場合、候補は `101..102` と `104..105` になり、最新側の `104..105` を優先する
- `--playlist-segments` は 6 以上とするが、`worker` が保持している連続範囲がそれより短い場合は、その短い範囲を公開することがある
- 複数のプレイリスト取得間でセグメントの連続再生は保証されない。最新範囲を保持していた `worker` が停止して古い範囲を持つ `worker` に切り替わった場合や、途中のセグメントが欠落した場合は、`#EXT-X-MEDIA-SEQUENCE` が前進するだけでなく、後退したり番号が飛んだりすることがある
- マスタープレイリストには、公開範囲と配信先 `worker` がある `quality` だけを列挙する。公開範囲がまだない `quality` や配信先 `worker` がいない `quality` は一時的に除外し、配信できる `quality` が 1 つもない場合は `503 Service Unavailable` を返す
- `publisher` が新しいセグメントを送らなくなって、最後の `PUT` から `duration_ms × --playlist-segments + --publish-wait-ms` を超えた `quality` は停止中とみなす。停止中の `quality` はマスタープレイリストから除外し、メディアプレイリストは `503 Service Unavailable` を返す
- 停止中も保存済みのセグメントと `worker` のキャッシュは削除しない。停止後に再開した場合は、停止前のセグメント番号を公開範囲とキャッシュ対象から除き、再開後の新しいセグメントだけを公開する

### ステータスコードの判定順

プレイリストはリクエストごとに現在の公開範囲から生成し、生成済みのプレイリストは保存しない。
ステータスコードは次の順序で判定する。

- マスタープレイリスト（`GET /channels/{channel_id}/master.m3u8`）：`400`、`404`、`302`、`503`（停止中または公開範囲なし）、`422`（公開範囲のファイル欠落）、`503`（配信先 `worker` なし）の順
- メディアプレイリスト（`GET /channels/{channel_id}/{quality}.m3u8`）：`400`、`404`、`503`（停止中または公開範囲なし）、`422`（公開範囲のファイル欠落）、`503`（配信先 `worker` なし）の順

公開範囲または配信先 `worker` がない場合は `503 Service Unavailable` を返し、代わりに `controller` 自身のセグメント URL をプレイリストに書くことはない。
`503 Service Unavailable` の応答には `Cache-Control: no-store` を付ける。
