#!/usr/bin/env python3
"""loop-publisher.py: mp4 を MPEG-TS に変換し、controller へ継続的に PUT するテスト用 publisher

同じ mp4 を無限に繰り返し、指定した長さのセグメントを controller に送り続ける。
セグメントごとにランダムな識別子を付け、controller が割り当てた番号を受け取る。
"""

from __future__ import annotations

import argparse
import http.client
import json
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
        description="mp4 を MPEG-TS に変換して controller へ継続的に PUT するテスト用 publisher",
    )
    parser.add_argument("--source", required=True, type=Path, help="入力 mp4 のパス")
    parser.add_argument("--controller-url", required=True, help="controller の絶対 http URL")
    parser.add_argument("--channel-id", required=True, help="channel の識別子")
    parser.add_argument("--quality", required=True, help="quality の識別子")
    parser.add_argument("--bandwidth", required=True, type=int, help="目標ビットレート（bit/s）")
    parser.add_argument(
        "--segment-duration-ms",
        required=True,
        type=int,
        help="セグメント長（ミリ秒で 500 以上）",
    )
    parser.add_argument("--resolution", help="controller へ宣言する解像度（WIDTHxHEIGHT）")
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
    validate_frame_rate(args, parser)


def validate_frame_rate(args, parser):
    """セグメント長に 1 フレーム以上含まれることを確認する"""
    try:
        result = subprocess.run(
            [
                "ffprobe",
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream=avg_frame_rate,r_frame_rate",
                "-of",
                "default=noprint_wrappers=1:nokey=1",
                str(args.source),
            ],
            capture_output=True,
            text=True,
        )
    except FileNotFoundError:
        parser.error("ffprobe not found in PATH")
    if result.returncode != 0:
        parser.error(f"ffprobe failed: {result.stderr.strip()}")

    frame_rate = 0.0
    for line in result.stdout.splitlines():
        numerator_text, separator, denominator_text = line.partition("/")
        if not separator:
            continue
        try:
            numerator = float(numerator_text)
            denominator = float(denominator_text)
        except ValueError:
            continue
        if numerator > 0 and denominator > 0:
            frame_rate = numerator / denominator
            break
    if frame_rate <= 0:
        parser.error("could not determine the input video frame rate")
    if frame_rate * args.segment_duration_ms / 1000 < 1:
        parser.error("segment-duration-ms is shorter than one frame of the input video")


def format_seconds(milliseconds):
    """ミリ秒を丸めずに ffmpeg が解釈できる秒の文字列にする"""
    whole, fraction = divmod(milliseconds, 1000)
    if fraction == 0:
        return str(whole)
    return f"{whole}.{fraction:03d}".rstrip("0")


def build_ffmpeg_command(args, work_dir, start_offset_ms):
    # セグメント長はミリ秒で受けるが、ffmpeg には丸めずに秒で渡す
    segment_seconds_text = format_seconds(args.segment_duration_ms)
    # 再起動時にタイムスタンプが巻き戻ると HLS client が再同期するため、次の番号の分だけ進める
    start_offset_text = format_seconds(start_offset_ms)
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
        "-c:v",
        "libx264",
        "-preset",
        "veryfast",
        "-pix_fmt",
        "yuv420p",
        "-b:v",
        str(args.bandwidth),
        "-maxrate",
        str(args.bandwidth),
        "-bufsize",
        str(args.bandwidth * 2),
        "-force_key_frames",
        f"expr:gte(t,n_forced*{segment_seconds_text})",
        "-c:a",
        "aac",
        "-b:a",
        "128k",
        "-ar",
        "48000",
        "-ac",
        "2",
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
    print(
        f"resend with: curl -X PUT --data-binary {shlex.quote(f'@{segment_path}')} "
        f"{shlex.quote(segment_url(args, identifier))}",
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


def run(args):
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
        args, work_dir, next_sequence * args.segment_duration_ms
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
    return run(args)


if __name__ == "__main__":
    sys.exit(main())
