#!/usr/bin/env python3
"""loop-publisher.py: mp4 をセグメントに分割し、controller へ継続的に PUT するテスト用 publisher

同じ mp4 を無限に繰り返し、指定した長さのセグメントを controller に送り続ける。
セグメントごとにランダムな識別子を付け、controller が割り当てた番号を受け取る。
再エンコードはしないため、素材にはセグメント長の倍数の位置にキーフレームが必要である。
"""

from __future__ import annotations

import argparse
import http.client
import json
import os
import re
import shlex
import signal
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
from bisect import bisect_left
from pathlib import Path

IDENTIFIER_PATTERN = re.compile(r"^[A-Za-z0-9_-]{1,64}$")
RESOLUTION_PATTERN = re.compile(r"^[0-9]+x[0-9]+$")
SEGMENT_FILE_PATTERN = re.compile(r"^([0-9]+)\.ts$")

PUT_TIMEOUT_SECONDS = 30
PLAYLIST_TIMEOUT_SECONDS = 5
POLL_INTERVAL_SECONDS = 0.2
FFMPEG_STOP_TIMEOUT_SECONDS = 5
PUT_ATTEMPTS = 3
PUT_RETRY_INTERVAL_SECONDS = 1
PLAYLIST_ATTEMPTS = 60
PLAYLIST_RETRY_INTERVAL_SECONDS = 0.5
CONNECTION_ERRORS = (urllib.error.URLError, OSError, http.client.HTTPException)

stop_requested = False


class PublisherError(Exception):
    """publisher を継続できない状態を表す"""


class PlaylistError(PublisherError):
    """プレイリストを取得できない状態を表す"""

    def __init__(self, message, status=None):
        super().__init__(message)
        self.status = status


def handle_stop_signal(_signum, _frame):
    """SIGINT と SIGTERM でループを止める"""
    global stop_requested
    stop_requested = True


def build_parser():
    parser = argparse.ArgumentParser(
        description="mp4 をセグメントに分割して controller へ継続的に PUT するテスト用 publisher",
    )
    parser.add_argument("--source", required=True, type=Path, help="入力 mp4 のパス")
    parser.add_argument("--controller-url", required=True, help="controller の絶対 http URL")
    parser.add_argument("--channel-id", required=True, help="channel の識別子")
    parser.add_argument("--quality", required=True, help="quality の識別子")
    parser.add_argument(
        "--bandwidth", required=True, type=int, help="controller へ宣言するビットレート（bit/s）"
    )
    parser.add_argument(
        "--segment-duration-ms",
        required=True,
        type=int,
        help="セグメント長（ミリ秒で 500 以上）",
    )
    parser.add_argument("--resolution", help="controller へ宣言する解像度（WIDTHxHEIGHT）")
    parser.add_argument("--auth-token", help="controller の PUT に使う Bearer トークン")
    return parser


def validate_args(args, parser):
    if not args.source.is_file():
        parser.error(f"source file does not exist: {args.source}")
    if IDENTIFIER_PATTERN.fullmatch(args.channel_id) is None:
        parser.error("channel_id must be 1 to 64 characters of [A-Za-z0-9_-]")
    if IDENTIFIER_PATTERN.fullmatch(args.quality) is None or args.quality == "master":
        parser.error("quality must be 1 to 64 characters of [A-Za-z0-9_-] and must not be master")
    if args.bandwidth <= 0:
        parser.error("bandwidth must be positive")
    if args.segment_duration_ms < 500:
        parser.error("segment-duration-ms must be at least 500")
    if args.resolution is not None and RESOLUTION_PATTERN.fullmatch(args.resolution) is None:
        parser.error("resolution must be WIDTHxHEIGHT")
    if args.auth_token is not None:
        # HTTP ヘッダーにそのまま載せられる値だけを受け付け、送信時の例外とトークンの取り違えを防ぐ
        if not args.auth_token or not all(
            0x21 <= ord(character) <= 0x7E for character in args.auth_token
        ):
            parser.error("auth-token must be non-empty printable ASCII without spaces")

    try:
        parsed_url = urllib.parse.urlsplit(args.controller_url)
        # ポート番号の形式もここで検証する
        _ = parsed_url.port
        urllib.request.Request(args.controller_url)
    except ValueError:
        parser.error("controller-url is not a valid URL")
    if parsed_url.scheme != "http" or parsed_url.hostname is None:
        parser.error("controller-url must be an absolute http URL")
    # 空の query と fragment も、後続の URL 組み立てを壊すため拒否する
    if "?" in args.controller_url or "#" in args.controller_url:
        parser.error("controller-url must not contain a query or fragment")
    args.controller_url = args.controller_url.rstrip("/")


def run_ffprobe(args, parser, options):
    try:
        return subprocess.run(
            ["ffprobe", "-v", "error", *options, str(args.source)],
            capture_output=True,
            text=True,
        )
    except FileNotFoundError:
        parser.error("ffprobe not found in PATH")


def probe_keyframe_times(args, parser):
    """素材にあるキーフレームの位置を返す"""
    result = run_ffprobe(
        args,
        parser,
        [
            "-select_streams",
            "v:0",
            "-skip_frame",
            "nokey",
            "-show_entries",
            "frame=pts_time",
            "-of",
            "csv=p=0",
        ],
    )
    if result.returncode != 0:
        parser.error(f"ffprobe failed: {result.stderr.strip()}")

    times = []
    for line in result.stdout.splitlines():
        try:
            times.append(float(line.strip().rstrip(",")))
        except ValueError:
            continue
    # open-GOP の素材では ffprobe がキーフレームを順不同で返すことがある
    times.sort()
    return times


def probe_duration_seconds(args, parser):
    """素材の長さを秒で返す"""
    result = run_ffprobe(
        args, parser, ["-show_entries", "format=duration", "-of", "csv=p=0"]
    )
    if result.returncode != 0:
        parser.error(f"ffprobe failed: {result.stderr.strip()}")
    try:
        return float(result.stdout.strip())
    except ValueError:
        parser.error("could not determine the source duration")


def probe_frame_duration_seconds(args, parser):
    """映像の 1 フレームの長さを秒で返す"""
    result = run_ffprobe(
        args,
        parser,
        ["-select_streams", "v:0", "-show_entries", "stream=r_frame_rate", "-of", "csv=p=0"],
    )
    if result.returncode != 0:
        parser.error(f"ffprobe failed: {result.stderr.strip()}")

    text = result.stdout.strip()
    numerator_text, separator, denominator_text = text.partition("/")
    try:
        numerator = float(numerator_text)
        denominator = float(denominator_text) if separator else 1.0
    except ValueError:
        parser.error("could not determine the input video frame rate")
    if numerator <= 0 or denominator <= 0:
        parser.error("could not determine the input video frame rate")
    return denominator / numerator


def probe_priming_seconds(args, parser):
    """素材の先頭にある負の PTS の大きさを秒で返す（無ければ 0）

    AAC はエンコーダの遅延（プリミング）を先頭のパケットの負の PTS として持つ。
    ffmpeg は 1 周ごとにタイムスタンプを余分に進めるため、周回の前進量は素材の
    長さよりこの値だけ大きくなる。終了時刻は素材の長さと一致するため、先頭の
    パケットだけを読む。
    """
    lowest = None
    for stream in ("v:0", "a:0"):
        result = run_ffprobe(
            args,
            parser,
            [
                "-select_streams",
                stream,
                # 先頭の 1 秒だけを読む。音声が無い素材では空になる
                "-read_intervals",
                "%+1",
                "-show_entries",
                "packet=pts_time",
                "-of",
                "csv=p=0",
            ],
        )
        if result.returncode != 0:
            parser.error(f"ffprobe failed: {result.stderr.strip()}")
        for line in result.stdout.splitlines():
            try:
                start = float(line.strip().rstrip(","))
            except ValueError:
                continue
            lowest = start if lowest is None else min(lowest, start)
    if lowest is None:
        parser.error("could not find a packet in the source")
    return max(0.0, -lowest)


def to_microseconds(seconds):
    """時刻をマイクロ秒に丸める

    ffprobe の時刻はマイクロ秒までの精度しかないため、境界の計算で生じた浮動小数点
    誤差を吸収できるよう、比較する時刻を同じ精度にそろえる。
    """
    return round(seconds * 1000000)


def find_cut(times, period, boundary):
    """周期的に繰り返すキーフレームのうち boundary 以降で最初の位置と番号を返す

    時刻はマイクロ秒の整数で扱い、切り位置が次の周回に入った場合も返す。
    """
    lap = boundary // period
    first_lap = lap
    while True:
        index = bisect_left(times, boundary - lap * period)
        if index < len(times):
            return lap * period + times[index], index, lap > first_lap
        lap += 1


def plan_segments(args, parser):
    """copy で指定した長さに分割できる素材か確認し、切り位置の計画を返す

    copy ではキーフレームの位置でしか分割できない。HLS muxer はセグメントの開始から
    セグメント長以上たった最初のキーフレームで切るため、キーフレームが境界から
    1 フレームを超えてずれると、前後のセグメントの長さが変わってしまう。
    同じキーフレームは周回ごとに前進量だけずれて現れるので、切り位置も同じだけずれる。
    戻り値は (先頭の切り位置, 1 周期分の切り位置, 1 周期の前進量, プリミング,
    素材の映像の先頭の時刻) で、いずれもマイクロ秒である。
    """
    segment = args.segment_duration_ms * 1000
    frame = to_microseconds(probe_frame_duration_seconds(args, parser))
    duration = to_microseconds(probe_duration_seconds(args, parser))
    priming = to_microseconds(probe_priming_seconds(args, parser))
    advance = duration + priming
    if segment < frame:
        parser.error("segment-duration-ms is shorter than one frame of the input video")
    if advance < segment:
        parser.error("source is shorter than one segment")

    times = [to_microseconds(time) for time in probe_keyframe_times(args, parser)]
    if not times:
        parser.error("could not find a keyframe in the source video")
    if times[0] > frame:
        parser.error("the source video must start with a keyframe")

    # muxer と同じ規則で切り位置をたどり、境界から 1 フレーム以内で切れるか確認する。
    # 切り位置がどのキーフレームになるかで次の切り位置も決まるため、同じキーフレームが
    # 再び現れるまでたどると、以降は同じ切り方を前進量だけずらして繰り返す
    cuts = [0]
    index = None
    seen = {}
    while index not in seen:
        seen[index] = len(cuts) - 1
        boundary = cuts[-1] + segment
        cut, index, next_lap = find_cut(times, advance, boundary)
        # 周回をまたぐ切り位置は次の周回の先頭のキーフレームになり、プリミングの分だけ
        # 境界より後ろになる。素材の長さの端数と同じく避けられないずれなので許容する
        allowed = frame + priming if next_lap else frame
        if cut - boundary > allowed:
            parser.error(
                f"source video has no keyframe within one frame of "
                f"{format_seconds(boundary // 1000)}s; "
                "keyframes must align with the segment duration"
            )
        cuts.append(cut)
    first = seen[index]
    return cuts[:first], cuts[first:-1], cuts[-1] - cuts[first], priming, times[0]


def segment_start_microseconds(plan, sequence):
    """指定した番号のセグメントが始まる素材内の時刻を返す"""
    head, cycle, stride, _, _ = plan
    if sequence < len(head):
        return head[sequence]
    sequence -= len(head)
    lap, position = divmod(sequence, len(cycle))
    return cycle[position] + lap * stride


def output_offset_microseconds(plan, sequence):
    """再起動した publisher が -output_ts_offset に渡す時刻を返す

    ffmpeg は実行ごとに出力の先頭の時刻を muxer の基準に合わせるため、素材の先頭から
    読む実行は、先頭パケットの負の PTS の分だけ後ろの時刻から始まる。プリミングを
    足して、再起動後の先頭の時刻を続きの時刻に合わせる。

    再起動した実行は素材の映像の先頭をもう一度読むため、映像が 0 以外から始まる
    素材では、その分だけ後ろにずれる。先頭の時刻を差し引いて合わせる。

    切り位置は各実行が素材の先頭から数え直すため、セグメントの長さと切り位置は
    実行をまたぐと素材の長さの端数、プリミング、キーフレームの量子化の分だけ
    変わりうる。ここで合わせるのは先頭の映像の時刻だけである。
    """
    return segment_start_microseconds(plan, sequence) + plan[3] - plan[4]


def format_microseconds(microseconds):
    """マイクロ秒を丸めずに ffmpeg が解釈できる秒の文字列にする

    負の値では divmod が返す整数部も負になるため、符号を外してから分ける。
    """
    sign = "-" if microseconds < 0 else ""
    whole, fraction = divmod(abs(microseconds), 1000000)
    if fraction == 0:
        return f"{sign}{whole}"
    return f"{sign}{whole}.{fraction:06d}".rstrip("0")


def format_seconds(milliseconds):
    """ミリ秒を丸めずに ffmpeg が解釈できる秒の文字列にする"""
    return format_microseconds(milliseconds * 1000)


def build_ffmpeg_command(args, work_dir, start_offset_microseconds):
    # セグメント長はミリ秒で受けるが、ffmpeg には丸めずに秒で渡す
    segment_seconds_text = format_seconds(args.segment_duration_ms)
    # 再起動時にタイムスタンプが巻き戻ると HLS client が再同期するため、続きの時刻から始める
    start_offset_text = format_microseconds(start_offset_microseconds)
    return [
        "ffmpeg",
        "-hide_banner",
        "-loglevel",
        "warning",
        "-re",
        "-stream_loop",
        "-1",
        "-i",
        str(args.source),
        "-map",
        "0:v:0",
        "-map",
        "0:a:0?",
        # 再エンコードは等倍に間に合わないためコピーする。区切りは素材のキーフレームに任せる
        "-c",
        "copy",
        "-output_ts_offset",
        start_offset_text,
        "-f",
        "hls",
        "-hls_time",
        segment_seconds_text,
        "-hls_list_size",
        "3",
        # temp_file は完成したセグメントだけを .ts に rename する。書き込み途中のファイルを
        # PUT しないために HLS muxer を使う
        "-hls_flags",
        "temp_file",
        "-hls_segment_type",
        "mpegts",
        "-hls_segment_filename",
        str(work_dir / "%d.ts"),
        str(work_dir / "index.m3u8"),
    ]


def ready_segments(work_dir):
    """完成した TS ファイルをファイル名の番号順に返す"""
    segments = []
    for path in work_dir.iterdir():
        match = SEGMENT_FILE_PATTERN.fullmatch(path.name)
        if match is None:
            continue
        segments.append((int(match.group(1)), path))
    segments.sort()
    return [path for _, path in segments]


def new_identifier():
    """セグメントごとに重ならない識別子を作る"""
    return uuid.uuid4().hex


def fetch_playlist(url):
    """プレイリストを取得する"""
    try:
        with urllib.request.urlopen(url, timeout=PLAYLIST_TIMEOUT_SECONDS) as response:
            return response.read().decode("utf-8", errors="replace")
    except urllib.error.HTTPError as error:
        raise PlaylistError(f"playlist returned status {error.code}", error.code) from error
    except CONNECTION_ERRORS as error:
        raise PlaylistError(f"playlist request failed: {error}") from error


def playlist_sequences(body):
    """メディアプレイリストに書かれたセグメント番号を返す"""
    sequences = []
    for line in body.splitlines():
        if not line.startswith("http"):
            continue
        name = line.rsplit("/", 1)[-1]
        if not name.endswith(".ts"):
            continue
        try:
            sequences.append(int(name[:-3]))
        except ValueError:
            continue
    return sequences


def fetch_playlist_with_retry(url, retry_statuses):
    """一時的な状態によるプレイリスト取得の失敗を再試行する"""
    for attempt in range(1, PLAYLIST_ATTEMPTS + 1):
        if stop_requested:
            raise PublisherError("stopped while waiting for the playlist")
        try:
            return fetch_playlist(url)
        except PlaylistError as error:
            if error.status not in retry_statuses:
                raise
            if attempt == PLAYLIST_ATTEMPTS:
                raise PublisherError(
                    f"playlist is not available after retries: {error}"
                ) from error
            print(
                f"waiting for the playlist ({error})",
                file=sys.stderr,
                flush=True,
            )
            time.sleep(PLAYLIST_RETRY_INTERVAL_SECONDS)


def next_sequence_from_playlist(args):
    """channel 全体の次のセグメント番号を master playlist から求める

    channel がまだ無い場合だけ 0 から採番される。quality だけが無い場合や他の quality が
    先行している場合も、既存 quality の最新番号の次に採番される。
    """
    master_url = f"{args.controller_url}/channels/{args.channel_id}/master.m3u8"
    try:
        # 422 と 503 は quality の配信範囲や worker の準備が整うまで待つ
        master = fetch_playlist_with_retry(master_url, (422, 503))
    except PlaylistError as error:
        # channel がまだ無い場合だけ 0 から採番される
        if error.status == 404:
            return 0
        raise

    next_sequence = 0
    for line in master.splitlines():
        if line.startswith("#") or not line.endswith(".m3u8"):
            continue
        quality = line[: -len(".m3u8")]
        media_url = f"{args.controller_url}/channels/{args.channel_id}/{quality}.m3u8"
        # master に載っている quality でも一時的に失敗することがあるため再試行する
        body = fetch_playlist_with_retry(media_url, (404, 422, 503))
        for sequence in playlist_sequences(body):
            next_sequence = max(next_sequence, sequence + 1)
    return next_sequence


def segment_url(args, identifier):
    query = {
        "duration_ms": str(args.segment_duration_ms),
        "bandwidth": str(args.bandwidth),
    }
    if args.resolution is not None:
        query["resolution"] = args.resolution
    return (
        f"{args.controller_url}/segments/{args.channel_id}/{args.quality}/{identifier}"
        f"?{urllib.parse.urlencode(query)}"
    )


def put_segment(args, identifier, segment_path):
    """セグメントを PUT し、controller が割り当てた番号を返す"""
    url = segment_url(args, identifier)
    request = urllib.request.Request(url, data=segment_path.read_bytes(), method="PUT")
    request.add_header("Content-Type", "application/octet-stream")
    if args.auth_token is not None:
        request.add_header("Authorization", f"Bearer {args.auth_token}")
    try:
        with urllib.request.urlopen(request, timeout=PUT_TIMEOUT_SECONDS) as response:
            status = response.status
            body = response.read().decode("utf-8", errors="replace").strip()
    except urllib.error.HTTPError as error:
        return error.code, None, error.read().decode("utf-8", errors="replace").strip()
    try:
        sequence = json.loads(body)["sequence"]
    except (json.JSONDecodeError, KeyError, TypeError) as error:
        # 応答が途中で切れた場合も同じファイルを再送すれば番号を受け取れる
        raise http.client.HTTPException(f"invalid response body: {body}") from error
    if not isinstance(sequence, int) or isinstance(sequence, bool) or sequence < 0:
        raise http.client.HTTPException(f"invalid sequence in response body: {body}")
    return status, sequence, body


def put_segment_with_retry(args, identifier, segment_path):
    """応答を受け取れなかった PUT は同じファイルを再送する

    controller は同じバイト列の再 PUT を 200 で受け付けるため、保存後に応答だけが
    失われた場合でも再送すれば状態を合わせられる。
    """
    attempt = 0
    last_error = None
    while True:
        if stop_requested:
            if last_error is None:
                return None
            # 再送待機中に停止した場合も、未確認のセグメントを残すために失敗として扱う
            raise last_error
        attempt += 1
        try:
            return put_segment(args, identifier, segment_path)
        except CONNECTION_ERRORS as error:
            if attempt >= PUT_ATTEMPTS or stop_requested:
                raise
            last_error = error
            print(
                f"retrying identifier {identifier} after a connection failure "
                f"({attempt}/{PUT_ATTEMPTS}): {error}",
                file=sys.stderr,
                flush=True,
            )
            time.sleep(PUT_RETRY_INTERVAL_SECONDS)


def print_resend_instructions(args, identifier, segment_path, reason):
    print(reason, file=sys.stderr, flush=True)
    print(f"the segment is kept at {segment_path}", file=sys.stderr, flush=True)
    curl_options = []
    if args.auth_token is not None:
        # トークンは値も引用符もログに出さず、curl だけが読む設定ファイルに書く
        config_path = segment_path.parent / "resend-curl.conf"
        escaped_token = args.auth_token.replace("\\", "\\\\").replace('"', '\\"')
        descriptor = os.open(config_path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(descriptor, "w", encoding="utf-8") as config:
            config.write(f'header = "Authorization: Bearer {escaped_token}"\n')
        curl_options.append(f"--config {shlex.quote(str(config_path))}")
    print(
        f"resend with: curl {' '.join([*curl_options, '-X', 'PUT'])} "
        f"--data-binary {shlex.quote(f'@{segment_path}')} "
        f"{shlex.quote(segment_url(args, identifier))}",
        file=sys.stderr,
        flush=True,
    )
    if args.auth_token is not None:
        print(
            "the config file contains the auth token; delete it after the resend",
            file=sys.stderr,
            flush=True,
        )
    print("restart the publisher after the resend succeeds", file=sys.stderr, flush=True)


def stop_ffmpeg(process):
    if process.poll() is not None:
        return
    process.terminate()
    try:
        process.wait(timeout=FFMPEG_STOP_TIMEOUT_SECONDS)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait()


def run(args, plan):
    global stop_requested
    stop_requested = False
    signal.signal(signal.SIGINT, handle_stop_signal)
    signal.signal(signal.SIGTERM, handle_stop_signal)

    work_dir = Path(tempfile.mkdtemp(prefix="loop-publisher-"))
    try:
        next_sequence = next_sequence_from_playlist(args)
    except PublisherError as error:
        print(
            f"failed to determine the next sequence: {error}",
            file=sys.stderr,
            flush=True,
        )
        shutil.rmtree(work_dir, ignore_errors=True)
        return 1
    print(f"work directory is {work_dir}", flush=True)
    print(f"next sequence is {next_sequence}", flush=True)
    command = build_ffmpeg_command(
        args, work_dir, output_offset_microseconds(plan, next_sequence)
    )
    print("starting ffmpeg: " + " ".join(command), flush=True)
    try:
        process = subprocess.Popen(command)
    except FileNotFoundError:
        print("ffmpeg not found in PATH", file=sys.stderr, flush=True)
        shutil.rmtree(work_dir, ignore_errors=True)
        return 1

    keep_work_dir = False
    try:
        while not stop_requested:
            segments = ready_segments(work_dir)
            if not segments:
                return_code = process.poll()
                if return_code is not None:
                    print(
                        f"ffmpeg exited with code {return_code}",
                        file=sys.stderr,
                        flush=True,
                    )
                    keep_work_dir = True
                    return 1
                time.sleep(POLL_INTERVAL_SECONDS)
                continue

            for segment_path in segments:
                if stop_requested:
                    break
                identifier = new_identifier()
                try:
                    result = put_segment_with_retry(args, identifier, segment_path)
                except CONNECTION_ERRORS as error:
                    print_resend_instructions(
                        args,
                        identifier,
                        segment_path,
                        f"failed to publish identifier {identifier}: {error}",
                    )
                    keep_work_dir = True
                    return 1
                if result is None:
                    break
                status, sequence, body = result
                if status not in (200, 201):
                    print_resend_instructions(
                        args,
                        identifier,
                        segment_path,
                        f"failed to publish identifier {identifier}: status {status} {body}",
                    )
                    keep_work_dir = True
                    return 1
                segment_path.unlink()
                print(
                    f"published identifier={identifier} sequence={sequence}",
                    flush=True,
                )
    finally:
        stop_ffmpeg(process)
        print("stopped", flush=True)
        if not keep_work_dir:
            shutil.rmtree(work_dir, ignore_errors=True)
    return 0


def main(argv=None):
    parser = build_parser()
    args = parser.parse_args(argv)
    validate_args(args, parser)
    plan = plan_segments(args, parser)
    return run(args, plan)


if __name__ == "__main__":
    sys.exit(main())
