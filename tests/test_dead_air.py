import dataclasses
import io
import json
import os
import sys
import tempfile
import threading
import time
import unittest
from email.message import Message
from pathlib import Path
from unittest.mock import patch

import dead_air


EXAMPLE = Path(__file__).parents[1] / "dead-air.example.toml"


def probe_json(*, audio_s=2.0, audio_start=0.0, video_s=2.0, video_gap=None,
               include_audio=True, include_video=True, format_duration_s=2.0):
    streams, frames = [], []
    if include_video:
        video_index = len(streams)
        streams.append({"index": video_index, "codec_type": "video", "time_base": "1/1000"})
        for frame_start in [index / 10 for index in range(int(video_s * 10))]:
            if video_gap and video_gap[0] <= frame_start < video_gap[1]:
                continue
            frames.append({"stream_index": video_index, "pts_time": str(frame_start),
                           "duration_time": "0.1"})
    if include_audio:
        audio_index = len(streams)
        streams.append({"index": audio_index, "codec_type": "audio", "sample_rate": "48000",
                        "time_base": "1/1000"})
        frames.append({"stream_index": audio_index, "pts_time": str(audio_start),
                       "nb_samples": int(audio_s * 48000)})
    return json.dumps({"streams": streams, "format": {"start_time": "0", "duration": str(format_duration_s)},
                       "frames": frames})


PROBE_AV = probe_json()
PROBE_CAPTURE_OFFSET_AV = probe_json(
    audio_start=0.1, video_s=2.1, format_duration_s=2.1,
)
PROBE_VIDEO = probe_json(include_audio=False)
PROBE_AUDIO = probe_json(include_video=False)
PROGRESS_2S = "frame=20\nout_time_us=2000000\nprogress=end\n"


def test_config(sample_s=2.0):
    base = dead_air.load_config(EXAMPLE)
    watch = dataclasses.replace(base.watch, sample_s=sample_s)
    stream = dataclasses.replace(
        base.streams[0], freeze_window_s=min(1.0, sample_s),
        silence_window_s=min(1.0, sample_s),
    )
    return dataclasses.replace(base, watch=watch, streams=(stream,))


def inspector_runner(probe=PROBE_AV, progress=PROGRESS_2S, logs="", probe_rc=0, decode_rc=0):
    calls = []

    def run(argv, timeout, **kwargs):
        calls.append(argv)
        if "-show_entries" in argv:
            return dead_air.ChildResult(probe, "", probe_rc)
        return dead_air.ChildResult(progress, logs, decode_rc)
    return run, calls


class ConfigTests(unittest.TestCase):
    def test_example_config_is_strict_and_valid(self):
        config = dead_air.load_config(EXAMPLE)
        self.assertEqual(config.watch.sample_s, 12)
        self.assertEqual(config.streams[0].source, "direct")
        self.assertEqual(config.network.private_allowlist, ())

    def test_unknown_config_key_is_rejected(self):
        import tomllib
        data = tomllib.loads(EXAMPLE.read_text(encoding="utf-8"))
        data["watch"]["extra"] = True
        with self.assertRaises(dead_air.ConfigError):
            dead_air.validate_config(data)

    def test_bad_youtube_host_is_rejected_without_network(self):
        import tomllib
        data = tomllib.loads(EXAMPLE.read_text(encoding="utf-8"))
        data["streams"][0]["source"] = "youtube"
        data["streams"][0]["url"] = "https://example.invalid/watch?v=demo"
        with self.assertRaises(dead_air.ConfigError):
            dead_air.validate_config(data)

    def test_youtube_thumbnail_fallback_can_derive_keyless_thumbnail(self):
        import tomllib
        data = tomllib.loads(EXAMPLE.read_text(encoding="utf-8"))
        stream = data["streams"][0]
        stream.update(source="youtube", url="https://youtu.be/AbCdEfGhijk",
                      thumbnail_enabled=True, thumbnail_url="")
        config = dead_air.validate_config(data)
        self.assertTrue(config.streams[0].thumbnail_enabled)
        self.assertEqual(dead_air._youtube_thumbnail_url(config.streams[0].url),
                         "https://i.ytimg.com/vi/AbCdEfGhijk/hqdefault.jpg")
        self.assertIsNone(dead_air._youtube_thumbnail_url("https://youtu.be/not-an-id"))


class ParserAndObservationTests(unittest.TestCase):
    def setUp(self):
        self.config = test_config()
        self.stream = self.config.streams[0]

    def test_probe_and_progress_capture_coverage(self):
        evidence = dead_air.parse_probe_json(PROBE_AV)
        progress = dead_air.parse_progress(PROGRESS_2S)
        self.assertEqual((evidence.audio_streams, evidence.video_streams), (1, 1))
        self.assertEqual(evidence.duration_s, 2.0)
        self.assertEqual((evidence.audio_index, evidence.video_index), (1, 0))
        self.assertTrue(progress.complete)
        self.assertEqual(progress.decoded_s, 2.0)
        self.assertEqual(progress.frames, 20)

    def test_decoded_coverage_rejects_delay_gaps_and_duplicate_overlap(self):
        delayed = dead_air.parse_probe_json(probe_json(audio_s=0.1, audio_start=1.9))
        delayed_coverage = dead_air._decoded_coverage(delayed, "AUDIO", 2.0)
        self.assertAlmostEqual(delayed_coverage.covered_s, 0.1, places=6)
        self.assertFalse(delayed_coverage.complete)

        gapped = dead_air.parse_probe_json(probe_json(video_gap=(0.8, 1.0)))
        video_coverage = dead_air._decoded_coverage(gapped, "VIDEO", 2.0)
        self.assertAlmostEqual(video_coverage.largest_gap_s, 0.2, places=6)
        self.assertFalse(video_coverage.complete)

        duplicate = json.dumps({
            "streams": [{"index": 0, "codec_type": "audio", "sample_rate": "48000",
                         "time_base": "1/1000"}],
            "format": {"start_time": "0", "duration": "2"},
            "frames": [{"stream_index": 0, "pts_time": "0", "nb_samples": 24000}] * 4,
        })
        duplicate_coverage = dead_air._decoded_coverage(
            dead_air.parse_probe_json(duplicate), "AUDIO", 2.0,
        )
        self.assertEqual(duplicate_coverage.covered_s, 0.5)
        self.assertEqual(duplicate_coverage.decoded_amount_s, 2.0)
        self.assertFalse(duplicate_coverage.complete)

        rounded_frames = []
        decoded_samples = 0
        for samples in [1024] * 140 + [640]:
            pts_ms = round(decoded_samples * 1000 / 48000)
            rounded_frames.append({
                "stream_index": 0, "media_type": "audio",
                "pts_time": f"{pts_ms / 1000:.3f}", "nb_samples": samples,
            })
            decoded_samples += samples
        rounded_probe = json.dumps({
            "streams": [{"index": 0, "codec_type": "audio", "sample_rate": "48000",
                         "time_base": "1/1000"}],
            "format": {"start_time": "0", "duration": "3"},
            "frames": rounded_frames,
        })
        rounded_coverage = dead_air._decoded_coverage(
            dead_air.parse_probe_json(rounded_probe), "AUDIO", 3.0,
        )
        self.assertEqual(rounded_coverage.frame_count, 141)
        self.assertEqual(rounded_coverage.sample_count, 140 * 1024 + 623)
        self.assertTrue(rounded_coverage.complete)

    def test_delayed_short_audio_stays_unknown_while_video_is_independently_healthy(self):
        config = test_config(sample_s=3.0)
        stream = config.streams[0]
        runner, calls = inspector_runner(
            probe=probe_json(audio_s=0.1, audio_start=2.9, video_s=3.0),
            progress="frame=30\nout_time_us=3000000\nprogress=end\n",
        )
        with tempfile.TemporaryDirectory() as directory:
            sample = Path(directory) / "delayed-public-safe.mkv"
            sample.write_bytes(b"fixture")
            result = dead_air.inspect_sample(sample, stream, config, runner=runner)
        self.assertEqual(result.audio.status, "UNKNOWN")
        self.assertEqual(result.video.status, "HEALTHY")
        self.assertAlmostEqual(result.audio.decoded_s, 0.1, places=6)
        self.assertAlmostEqual(result.video.decoded_s, 3.0, places=6)
        condition_evidence = dead_air._transition_sample("audio_silence", result)
        coverage = condition_evidence["dimensions"]["audio"]["coverage"]
        self.assertEqual(coverage["complete"], False)
        self.assertAlmostEqual(coverage["covered_s"], 0.1, places=3)
        self.assertEqual((coverage["sample_count"], coverage["sample_rate_hz"]), (4800, 48000))
        self.assertNotIn("frames", coverage)
        probe_argv = calls[0]
        self.assertIn("-show_frames", probe_argv)
        self.assertNotIn("-count_frames", probe_argv)

    def test_intervals_include_start_end_and_open_end_at_decode_coverage(self):
        freezes, silences = dead_air._parse_intervals(
            "lavfi.freezedetect.freeze_start: 0.25\n"
            "lavfi.freezedetect.freeze_end: 1.50\n"
            "silence_start: 0.5\n",
            2.0,
        )
        self.assertEqual((freezes[0].start_s, freezes[0].end_s), (0.25, 1.5))
        self.assertEqual((silences[0].start_s, silences[0].end_s), (0.5, 2.0))
        self.assertTrue(silences[0].open_at_sample_end)

    def test_freeze_is_video_unhealthy_not_off_air(self):
        runner, _ = inspector_runner(logs="lavfi.freezedetect.freeze_start: 0.0\nlavfi.freezedetect.freeze_end: 1.5\n")
        with tempfile.TemporaryDirectory() as directory:
            sample = Path(directory) / "sample.mkv"
            sample.write_bytes(b"fixture")
            result = dead_air.inspect_sample(sample, self.stream, self.config, runner=runner)
        self.assertEqual(result.status, "UNHEALTHY")
        self.assertEqual(result.video.status, "UNHEALTHY")
        self.assertEqual(result.video.reason_codes, ("VIDEO_FREEZE",))
        self.assertEqual(result.audio.status, "HEALTHY")
        self.assertNotEqual(result.status, "OFF_AIR")

    def test_silence_is_audio_unhealthy_and_dimensions_are_separate(self):
        runner, _ = inspector_runner(logs="silence_start: 0.0\nsilence_end: 1.4 | silence_duration: 1.4\n")
        with tempfile.TemporaryDirectory() as directory:
            sample = Path(directory) / "sample.mkv"
            sample.write_bytes(b"fixture")
            result = dead_air.inspect_sample(sample, self.stream, self.config, runner=runner)
        self.assertEqual(result.audio.status, "UNHEALTHY")
        self.assertEqual(result.video.status, "HEALTHY")
        self.assertEqual(result.status, "UNHEALTHY")
        self.assertNotEqual(result.status, "OFF_AIR")

    def test_both_events_report_both_dimensions(self):
        logs = (
            "lavfi.freezedetect.freeze_start: 0.0\nlavfi.freezedetect.freeze_end: 1.5\n"
            "silence_start: 0.0\nsilence_end: 1.5 | silence_duration: 1.5\n"
        )
        runner, _ = inspector_runner(logs=logs)
        with tempfile.TemporaryDirectory() as directory:
            sample = Path(directory) / "sample.mkv"
            sample.write_bytes(b"fixture")
            result = dead_air.inspect_sample(sample, self.stream, self.config, runner=runner)
        self.assertEqual((result.video.status, result.audio.status), ("UNHEALTHY", "UNHEALTHY"))
        self.assertEqual(set(result.reason_codes), {"VIDEO_FREEZE", "AUDIO_SILENCE"})

    def test_missing_required_audio_stays_unknown_while_video_can_be_proven(self):
        runner, _ = inspector_runner(probe=PROBE_VIDEO)
        with tempfile.TemporaryDirectory() as directory:
            sample = Path(directory) / "sample.mkv"
            sample.write_bytes(b"fixture")
            result = dead_air.inspect_sample(sample, self.stream, self.config, runner=runner)
        self.assertEqual(result.audio.status, "UNKNOWN")
        self.assertEqual(result.audio.reason_codes, ("AUDIO_MISSING",))
        self.assertEqual(result.video.status, "HEALTHY")
        self.assertEqual(result.status, "UNKNOWN")

    def test_configured_audio_only_stream_can_be_healthy(self):
        stream = dataclasses.replace(self.stream, expect_video=False, detect_freeze=False)
        config = dataclasses.replace(self.config, streams=(stream,))
        runner, _ = inspector_runner(probe=PROBE_AUDIO, progress="frame=0\nout_time_us=2000000\nprogress=end\n")
        with tempfile.TemporaryDirectory() as directory:
            sample = Path(directory) / "audio.mkv"
            sample.write_bytes(b"fixture")
            result = dead_air.inspect_sample(sample, stream, config, runner=runner)
        self.assertEqual(result.status, "HEALTHY")
        self.assertEqual(result.audio.status, "HEALTHY")
        self.assertEqual(result.video.status, "NOT_APPLICABLE")

    def test_short_capture_and_corrupt_sample_are_unknown(self):
        short_probe = probe_json(audio_s=0.4, video_s=0.4)
        runner, _ = inspector_runner(probe=short_probe, progress="frame=4\nout_time_us=400000\nprogress=end\n")
        with tempfile.TemporaryDirectory() as directory:
            sample = Path(directory) / "short.mkv"
            sample.write_bytes(b"fixture")
            result = dead_air.inspect_sample(sample, self.stream, self.config, runner=runner)
        self.assertEqual((result.audio.status, result.video.status), ("UNKNOWN", "UNKNOWN"))

        corrupt_runner, _ = inspector_runner(probe="not json")
        with tempfile.TemporaryDirectory() as directory:
            sample = Path(directory) / "corrupt.mkv"
            sample.write_bytes(b"fixture")
            result = dead_air.inspect_sample(sample, self.stream, self.config, runner=corrupt_runner)
        self.assertEqual(result.status, "UNKNOWN")

    def test_explicit_direct_offline_contract_is_separate_from_media_events(self):
        result = dead_air.HttpResult(410, "https://example.invalid/live/sample", "", b"")
        self.assertEqual(dead_air._off_air_from_direct(result, self.stream), "DIRECT_OFFLINE_STATUS")
        generic_404 = dataclasses.replace(result, status=404)
        self.assertIsNone(dead_air._off_air_from_direct(generic_404, self.stream))

    def test_hls_endlist_requires_successful_playlist_and_tag_line(self):
        url = "https://example.invalid/live/list.m3u8"
        for status, body in (
            (403, b"#EXTM3U\n#EXT-X-ENDLIST\n"),
            (500, b"server error #EXT-X-ENDLIST"),
            (200, b"blocked page #EXT-X-ENDLIST"),
            (200, b"#EXTM3U\n# a comment mentions #EXT-X-ENDLIST\n"),
        ):
            self.assertIsNone(dead_air._off_air_from_direct(
                dead_air.HttpResult(status, url, "application/vnd.apple.mpegurl", body), self.stream,
            ))
        genuine = dead_air.HttpResult(
            200, url, "application/vnd.apple.mpegurl",
            b"#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-ENDLIST\n",
        )
        self.assertEqual(dead_air._off_air_from_direct(genuine, self.stream), "DIRECT_HLS_ENDLIST")
        failed_read = dataclasses.replace(genuine, error_code="HTTP_READ_FAILED")
        self.assertIsNone(dead_air._off_air_from_direct(failed_read, self.stream))


class ProcessAndPolicyTests(unittest.TestCase):
    def setUp(self):
        self.config = test_config()

    def _watch_observation(self, status, index):
        stream = self.config.streams[0]
        if status == "UNHEALTHY":
            audio = dead_air.DimensionEvidence(
                "UNHEALTHY", ("AUDIO_SILENCE",), sampled_s=2, decoded_s=2,
            )
            video = dead_air.DimensionEvidence(
                "HEALTHY", ("VIDEO_DECODED_NO_EVENT",), sampled_s=2, decoded_s=2,
            )
            reasons = ("AUDIO_SILENCE",)
        elif status == "UNKNOWN":
            audio = dead_air.DimensionEvidence("UNKNOWN", ("AUDIO_UNKNOWN",))
            video = dead_air.DimensionEvidence("UNKNOWN", ("VIDEO_UNKNOWN",))
            reasons = ("AUDIO_UNKNOWN", "VIDEO_UNKNOWN")
        else:
            audio = dead_air.DimensionEvidence(
                "HEALTHY", ("AUDIO_DECODED_NO_EVENT",), sampled_s=2, decoded_s=2,
            )
            video = dead_air.DimensionEvidence(
                "HEALTHY", ("VIDEO_DECODED_NO_EVENT",), sampled_s=2, decoded_s=2,
            )
            reasons = ()
        return dead_air.Observation(
            stream.id, f"2026-10-23T00:00:{index:02d}Z", status, reasons, audio, video,
        )

    def test_ytdlp_argv_is_fixed_and_positionally_safe(self):
        url = "https://www.youtube.com/watch?v=example"
        argv = dead_air.build_ytdlp_argv("yt-dlp", url)
        self.assertEqual(argv[-2:], ["--", url])
        self.assertIn("--ignore-config", argv)
        self.assertIn("--no-plugin-dirs", argv)
        self.assertNotIn("--cookies", argv)

    def test_remote_capture_protocol_options_precede_each_input_without_file(self):
        media = dead_air.ResolvedMedia((
            dead_air.MediaInput("video", "https://cdn.invalid/video"),
            dead_air.MediaInput("audio", "https://cdn.invalid/audio"),
        ))
        argv = dead_air.build_capture_argv(
            media, self.config.streams[0], self.config, Path("sample.mkv"),
        )
        self.assertNotIn("file", dead_air.INPUT_PROTOCOLS.split(","))
        input_indexes = [i for i, value in enumerate(argv) if value == "-i"]
        self.assertEqual(len(input_indexes), 2)
        for index in input_indexes:
            self.assertEqual(argv[index - 6:index], [
                "-protocol_whitelist", dead_air.INPUT_PROTOCOLS,
                "-tls_verify", "1",
                "-rw_timeout", "10000000",
            ])

    def test_fake_resolver_and_capture_run_without_external_transport(self):
        stream = dataclasses.replace(
            self.config.streams[0], source="youtube",
            url="https://www.youtube.com/watch?v=fixture-id",
        )
        config = dataclasses.replace(self.config, streams=(stream,))
        calls = []
        resolver_json = json.dumps({
            "live_status": "is_live",
            "formats": [{"url": "https://cdn.invalid/media", "vcodec": "avc1", "acodec": "aac", "height": 360}],
        })

        def runner(argv, timeout, **kwargs):
            calls.append(argv)
            if argv[0] == "yt-dlp":
                return dead_air.ChildResult(resolver_json, "", 0)
            if "-show_entries" in argv:
                return dead_air.ChildResult(PROBE_CAPTURE_OFFSET_AV, "", 0)
            if "-f" in argv and "matroska" in argv:
                Path(argv[-1]).write_bytes(b"bounded sample")
                return dead_air.ChildResult("", "", 0)
            return dead_air.ChildResult(PROGRESS_2S, "", 0)

        def unexpected_transport(*_args, **_kwargs):
            raise AssertionError("YouTube path must use the injected resolver, not Python HTTP")

        result = dead_air.observe_stream(
            config, stream, runner=runner,
            address_resolver=lambda _host, _port: ["93.184.216.34"],
            transport=unexpected_transport,
        )
        self.assertEqual(result.status, "HEALTHY")
        self.assertEqual(len(calls), 5)
        self.assertIn("--no-remote-components", calls[0])
        capture = next(argv for argv in calls if "-rw_timeout" in argv)
        self.assertIn("-threads", capture)
        self.assertIn("1", capture)
        self.assertIn("-protocol_whitelist", capture)

    def test_resolver_unknown_uses_one_validated_youtube_thumbnail_fallback(self):
        stream = dataclasses.replace(
            self.config.streams[0], source="youtube",
            url="https://www.youtube.com/watch?v=AbCdEfGhijk",
            thumbnail_enabled=True, thumbnail_url="",
        )
        config = dataclasses.replace(self.config, streams=(stream,))
        resolver_calls, thumbnail_reads = [], []

        def runner(argv, _timeout, **_kwargs):
            resolver_calls.append(argv)
            return dead_air.ChildResult("", "", 2)

        def transport(url, **_kwargs):
            thumbnail_reads.append(url)
            return dead_air.HttpResult(200, url, "image/jpeg", b"public-safe-thumbnail")

        result = dead_air.observe_stream(
            config, stream, runner=runner,
            address_resolver=lambda _host, _port: ["93.184.216.34"],
            thumbnail_history={}, transport=transport,
        )
        self.assertEqual(len(resolver_calls), 1)
        self.assertEqual(thumbnail_reads, [
            "https://i.ytimg.com/vi/AbCdEfGhijk/hqdefault.jpg",
        ])
        self.assertEqual((result.status, result.video.status, result.audio.status),
                         ("DEGRADED", "DEGRADED", "UNKNOWN"))
        self.assertIn("FULL_MEDIA_UNVERIFIED", result.reason_codes)

    def test_resolver_off_air_policy_failure_and_exhausted_budget_skip_fallback(self):
        stream = dataclasses.replace(
            self.config.streams[0], source="youtube",
            url="https://www.youtube.com/watch?v=AbCdEfGhijk",
            thumbnail_enabled=True, thumbnail_url="",
        )
        config = dataclasses.replace(self.config, streams=(stream,))
        transports = []

        def transport(url, **_kwargs):
            transports.append(url)
            return dead_air.HttpResult(200, url, "image/jpeg", b"thumbnail")

        off_air_stream = dataclasses.replace(stream, expected_live=True)
        off_air = dead_air.observe_stream(
            config, off_air_stream,
            runner=lambda *_args, **_kwargs: dead_air.ChildResult(
                json.dumps({"live_status": "is_upcoming"}), "", 0,
            ),
            address_resolver=lambda _host, _port: ["93.184.216.34"],
            thumbnail_history={}, transport=transport,
        )
        self.assertEqual(off_air.status, "OFF_AIR")

        blocked_payload = json.dumps({"live_status": "is_live", "formats": [{
            "url": "https://media.invalid/blocked", "vcodec": "avc1", "acodec": "aac",
        }]})
        blocked = dead_air.observe_stream(
            config, stream,
            runner=lambda *_args, **_kwargs: dead_air.ChildResult(blocked_payload, "", 0),
            address_resolver=lambda host, _port: (
                ["10.0.0.8"] if host == "media.invalid" else ["93.184.216.34"]
            ),
            thumbnail_history={}, transport=transport,
        )
        self.assertEqual(blocked.reason_codes, ("RESOLVER_MEDIA_URL_REJECTED",))

        exhausted = dead_air.observe_stream(
            config, stream,
            runner=lambda *_args, **_kwargs: self.fail("exhausted budget must stop before resolver"),
            address_resolver=lambda _host, _port: ["93.184.216.34"],
            thumbnail_history={}, deadline=time.monotonic() - 1, transport=transport,
        )
        self.assertEqual(exhausted.reason_codes, ("CYCLE_BUDGET_EXHAUSTED",))
        self.assertEqual(transports, [])

    def test_fake_direct_transport_confirms_only_explicit_offline_contract(self):
        stream = self.config.streams[0]
        calls = []
        def transport(url, **kwargs):
            calls.append(url)
            return dead_air.HttpResult(410, url, "", b"")
        result = dead_air.observe_stream(
            self.config, stream, runner=lambda *_args, **_kwargs: self.fail("off-air must stop before capture"),
            address_resolver=lambda _host, _port: ["93.184.216.34"],
            transport=transport,
        )
        self.assertEqual(result.status, "OFF_AIR")
        self.assertEqual(result.reason_codes, ("DIRECT_OFFLINE_STATUS",))
        self.assertEqual(len(calls), 1)

    def test_short_track_keeps_unknown_while_long_other_track_is_healthy(self):
        def runner(argv, _timeout, **_kwargs):
            if "-show_entries" in argv:
                return dead_air.ChildResult(probe_json(audio_s=0.4), "", 0)
            if "0:a:0" in argv:
                return dead_air.ChildResult(
                    "frame=0\nout_time_us=400000\nprogress=end\n",
                    "silence_start: 0.0\n", 0,
                )
            return dead_air.ChildResult(PROGRESS_2S, "", 0)

        with tempfile.TemporaryDirectory() as directory:
            sample = Path(directory) / "asymmetric.mkv"
            sample.write_bytes(b"fixture")
            result = dead_air.inspect_sample(sample, self.config.streams[0], self.config, runner=runner)
        self.assertEqual(result.audio.status, "UNKNOWN")
        self.assertEqual(result.video.status, "HEALTHY")
        self.assertEqual(result.audio.decoded_s, 0.4)
        self.assertEqual(result.video.decoded_s, 2.0)

    def test_thumbnail_fallback_stays_degraded_and_audio_unknown(self):
        stream = dataclasses.replace(
            self.config.streams[0], thumbnail_enabled=True,
            thumbnail_url="https://images.invalid/thumb.png",
        )
        config = dataclasses.replace(self.config, streams=(stream,))
        calls = []
        def runner(argv, timeout, **kwargs):
            calls.append(argv)
            if "-f" in argv and "matroska" in argv:
                return dead_air.ChildResult("", "", 0)  # no sample file: force honest fallback
            return dead_air.ChildResult("", "", 1)
        def transport(url, **kwargs):
            if url == stream.url:
                return dead_air.HttpResult(200, url, "video/mp2t", b"prefix")
            return dead_air.HttpResult(200, url, "image/png", b"synthetic-thumbnail")
        result = dead_air.observe_stream(
            config, stream, runner=runner,
            address_resolver=lambda _host, _port: ["93.184.216.34"],
            thumbnail_history={}, transport=transport,
        )
        self.assertEqual(result.status, "DEGRADED")
        self.assertEqual(result.video.status, "DEGRADED")
        self.assertEqual(result.audio.status, "UNKNOWN")
        self.assertIn("FULL_MEDIA_UNVERIFIED", result.reason_codes)
        self.assertTrue(calls)

    def test_youtube_thumbnail_fallback_uses_validated_id_without_configured_url(self):
        stream = dataclasses.replace(
            self.config.streams[0], source="youtube",
            url="https://www.youtube.com/watch?v=AbCdEfGhijk",
            thumbnail_enabled=True, thumbnail_url="",
        )
        calls = []
        original = dead_air._unknown_observation(stream, "CAPTURE_FAILED")
        result = dead_air._fallback_observation(
            original, stream, self.config, {},
            address_resolver=lambda host, _port: (calls.append(host) or ["93.184.216.34"]),
            now=100,
            transport=lambda url, **_kwargs: (
                calls.append(url) or dead_air.HttpResult(200, url, "image/jpeg", b"thumbnail")
            ),
        )
        self.assertIn("i.ytimg.com", calls)
        self.assertIn("https://i.ytimg.com/vi/AbCdEfGhijk/hqdefault.jpg", calls)
        self.assertEqual((result.status, result.audio.status, result.video.status),
                         ("DEGRADED", "UNKNOWN", "DEGRADED"))
        self.assertIn("FULL_MEDIA_UNVERIFIED", result.reason_codes)

    def test_unchanged_thumbnail_ages_from_last_change(self):
        stream = dataclasses.replace(
            self.config.streams[0], thumbnail_enabled=True,
            thumbnail_url="https://images.invalid/thumb.png", thumbnail_stale_after_s=60,
        )
        config = dataclasses.replace(self.config, streams=(stream,))
        history = {}
        def runner(argv, timeout, **kwargs):
            if "-f" in argv and "matroska" in argv:
                return dead_air.ChildResult("", "", 0)
            return dead_air.ChildResult("", "", 1)
        def transport(url, **kwargs):
            if url == stream.url:
                return dead_air.HttpResult(200, url, "video/mp2t", b"prefix")
            return dead_air.HttpResult(200, url, "image/png", b"unchanged")
        with patch.object(dead_air.time, "monotonic", side_effect=[1000.0, 1061.0]):
            first = dead_air.observe_stream(
                config, stream, runner=runner,
                address_resolver=lambda _host, _port: ["93.184.216.34"],
                thumbnail_history=history, transport=transport,
            )
            second = dead_air.observe_stream(
                config, stream, runner=runner,
                address_resolver=lambda _host, _port: ["93.184.216.34"],
                thumbnail_history=history, transport=transport,
            )
        self.assertIn("THUMBNAIL_BASELINE_ONLY", first.reason_codes)
        self.assertIn("THUMBNAIL_SUSPECT_STATIC", second.reason_codes)
        self.assertEqual(second.status, "DEGRADED")

    def test_private_dns_answers_are_denied_unless_exactly_allowed(self):
        url = "https://example.invalid/live/sample"
        answers = lambda _host, _port: ["10.0.0.8"]
        with self.assertRaises(dead_air.PolicyError):
            dead_air.validate_network_target(url, "direct", self.config.network, False, answers)
        network = dataclasses.replace(self.config.network, private_allowlist=("example.invalid:443",))
        accepted = dead_air.validate_network_target(url, "direct", network, True, answers)
        self.assertEqual(accepted, ("10.0.0.8",))

    def test_metadata_address_is_always_denied(self):
        url = "https://example.invalid/live/sample"
        network = dataclasses.replace(self.config.network, private_allowlist=("example.invalid:443",))
        with self.assertRaises(dead_air.PolicyError):
            dead_air.validate_network_target(url, "direct", network, True,
                                             lambda _host, _port: ["169.254.169.254"])

    def test_child_environment_excludes_proxy_and_credentials(self):
        with patch.dict(os.environ, {"HTTP_PROXY": "secret-proxy", "API_TOKEN": "secret"}, clear=True):
            env = dead_air.child_environment()
        self.assertNotIn("HTTP_PROXY", env)
        self.assertNotIn("API_TOKEN", env)
        self.assertEqual(env["LC_ALL"], "C")

    def test_timeout_terminates_and_reaps_child_with_finite_wait(self):
        result = dead_air.run_bounded(
            [sys.executable, "-c", "import time; time.sleep(5)"],
            0.1, max_output_bytes=256,
        )
        self.assertTrue(result.timed_out)
        self.assertTrue(result.cleanup_complete)
        self.assertIsNotNone(result.returncode)

    def test_normal_exit_drains_final_detector_marker_before_closing_pipes(self):
        exited = threading.Event()

        class DelayedPipe:
            def __init__(self, value):
                self.value = value
                self.read_once = False
                self.closed = False

            def read(self, _size):
                self.assert_exit()
                if not self.read_once:
                    self.read_once = True
                    return self.value
                return b""

            def assert_exit(self):
                if not exited.wait(1):
                    raise OSError("child did not exit before buffered bytes became readable")

            def close(self):
                self.closed = True

        class ExitedChild:
            returncode = 0
            pid = 12345

            def __init__(self):
                self.stdout = DelayedPipe(b"progress=end\n")
                self.stderr = DelayedPipe(b"lavfi.freezedetect.freeze_start: 0.0\n")

            def poll(self):
                exited.set()
                return self.returncode

        child = ExitedChild()
        with patch.object(dead_air.subprocess, "Popen", return_value=child), \
                patch.object(dead_air.os, "killpg", create=True):
            result = dead_air.run_bounded([sys.executable, "-c", "pass"], 1, max_output_bytes=256)
        self.assertTrue(result.cleanup_complete)
        self.assertIn("freeze_start", result.stderr)
        self.assertEqual(result.stdout, "progress=end\n")

    def test_keyboard_interrupt_terminates_and_reaps_started_child(self):
        original_popen = dead_air.subprocess.Popen
        original_sleep = time.sleep
        spawned = []

        def tracked_popen(*args, **kwargs):
            child = original_popen(*args, **kwargs)
            spawned.append(child)
            return child

        interrupt = [True]
        def interrupt_once(_seconds):
            if interrupt[0]:
                interrupt[0] = False
                raise KeyboardInterrupt
            original_sleep(0.005)

        with patch.object(dead_air.subprocess, "Popen", side_effect=tracked_popen), \
                patch.object(dead_air.time, "sleep", side_effect=interrupt_once):
            with self.assertRaises(KeyboardInterrupt):
                dead_air.run_bounded(
                    [sys.executable, "-c", "import time; time.sleep(30)"],
                    5, max_output_bytes=256,
                )
        self.assertEqual(len(spawned), 1)
        self.assertIsNotNone(spawned[0].poll(), "started direct child must be reaped")

    def test_http_operation_timeout_caps_redirects_and_body_reads(self):
        now = [100.0]
        open_timeouts = []
        read_timeouts = []

        class FakeSocket:
            def settimeout(self, value):
                read_timeouts.append(value)

        class FakeResponse:
            def __init__(self, status, location=None):
                self.status = status
                self.headers = Message()
                self.headers["Content-Type"] = "text/plain"
                if location:
                    self.headers["Location"] = location
                self.fp = type("FP", (), {"raw": type("Raw", (), {"_sock": FakeSocket()})()})()
                self.reads = 0

            def read1(self, _amount):
                self.reads += 1
                now[0] += 0.1
                return b"x" if self.reads == 1 else b""

            def read(self, amount):
                return self.read1(amount)

            def close(self):
                pass

        class FakeOpener:
            responses = [
                FakeResponse(302, "https://example.invalid/next"),
                FakeResponse(200),
            ]

            def open(self, _request, timeout):
                open_timeouts.append(timeout)
                return self.responses.pop(0)

        def validate(*_args, **_kwargs):
            now[0] += 0.25  # target checks consume the same operation deadline

        with patch.object(dead_air.time, "monotonic", side_effect=lambda: now[0]), \
                patch.object(dead_air, "validate_network_target", side_effect=validate), \
                patch.object(dead_air.urllib.request, "build_opener", return_value=FakeOpener()):
            result = dead_air.bounded_http_get(
                "https://example.invalid/start", source="direct", network=self.config.network,
                allow_private=False, timeout_s=2, max_bytes=64, deadline=150,
            )
        self.assertEqual((result.status, result.body), (200, b"x"))
        self.assertEqual(len(open_timeouts), 2)
        self.assertTrue(all(0 < value <= 2 for value in open_timeouts))
        self.assertLess(open_timeouts[1], open_timeouts[0])
        self.assertTrue(read_timeouts)
        self.assertTrue(all(0 < value <= 2 for value in read_timeouts))

    def test_short_http_operation_leaves_cycle_budget_for_later_stream(self):
        now = [100.0]
        open_timeouts = []

        class Response:
            status = 200
            headers = Message()
            headers["Content-Type"] = "text/plain"

            def read1(self, _amount):
                return b""

            def read(self, amount):
                return self.read1(amount)

            def close(self):
                pass

        class SlowOpener:
            def open(self, _request, timeout):
                open_timeouts.append(timeout)
                now[0] += timeout
                return Response()

        first = self.config.streams[0]
        second = dataclasses.replace(first, id="later")
        watch = dataclasses.replace(self.config.watch, cycle_budget_s=30)
        config = dataclasses.replace(self.config, watch=watch, streams=(first, second))
        observed = []

        def observe(_config, stream, **kwargs):
            observed.append((stream.id, kwargs["deadline"]))
            if stream.id == first.id:
                dead_air.bounded_http_get(
                    "https://example.invalid/start", source="direct", network=config.network,
                    allow_private=False, timeout_s=2, max_bytes=64,
                    deadline=kwargs["deadline"],
                )
            return dead_air._unknown_observation(stream, "FIXTURE")

        def validate(*_args, **_kwargs):
            now[0] += 0.1

        with patch.object(dead_air.time, "monotonic", side_effect=lambda: now[0]), \
                patch.object(dead_air, "validate_network_target", side_effect=validate), \
                patch.object(dead_air.urllib.request, "build_opener", return_value=SlowOpener()), \
                patch.object(dead_air, "observe_stream", side_effect=observe):
            results = dead_air.check_cycle(config, clock=lambda: now[0])
        self.assertEqual(len(results), 2)
        self.assertEqual([entry[0] for entry in observed], [first.id, second.id])
        self.assertLessEqual(open_timeouts[0], 2)
        self.assertGreater(observed[1][1] - now[0], 27)

    def test_output_overflow_is_capped_and_terminates_child(self):
        result = dead_air.run_bounded(
            [sys.executable, "-c", "import sys; sys.stdout.write('x' * 8192)"],
            2, max_output_bytes=128,
        )
        self.assertTrue(result.output_limited)
        self.assertLessEqual(len(result.stdout.encode("utf-8")), 128)
        self.assertTrue(result.cleanup_complete)

    def test_watch_cycles_are_serial_and_bounded_by_requested_count(self):
        observed = []
        sleeps = []
        def cycle(config):
            observed.append(config)
            stream = config.streams[0]
            return (dead_air.Observation(
                stream.id, "2026-10-23T00:00:00Z", "HEALTHY", (),
                dead_air.DimensionEvidence("HEALTHY", ("AUDIO_DECODED_NO_EVENT",)),
                dead_air.DimensionEvidence("HEALTHY", ("VIDEO_DECODED_NO_EVENT",)),
            ),)
        cycles = list(dead_air.watch_cycles(self.config, 3, cycle=cycle,
                                           sleeper=sleeps.append, clock=lambda: 100.0))
        self.assertEqual(len(cycles), 3)
        self.assertEqual(len(observed), 3)
        self.assertEqual(sleeps, [self.config.watch.interval_s] * 2)

    def test_watch_requires_one_explicit_mode_and_rejects_invalid_count_before_state(self):
        invalid_modes = (
            ["watch", "--config", "unused.toml"],
            ["watch", "--config", "unused.toml", "--max-cycles", "3", "--continuous"],
        )
        for argv in invalid_modes:
            with self.subTest(argv=argv), patch("sys.stderr", new=io.StringIO()), \
                    patch.object(dead_air, "load_config") as load, \
                    patch.object(dead_air, "IncidentStore") as store:
                with self.assertRaises(SystemExit) as raised:
                    dead_air.main(argv)
                self.assertEqual(raised.exception.code, 2)
                load.assert_not_called()
                store.assert_not_called()

        printed = []
        with patch.object(dead_air, "load_config") as load, \
                patch.object(dead_air, "IncidentStore") as store, \
                patch("builtins.print", side_effect=lambda *args, **kwargs: printed.append((args, kwargs))):
            result = dead_air.main([
                "watch", "--config", "unused.toml", "--max-cycles", "0",
            ])
        self.assertEqual(result, 3)
        load.assert_not_called()
        store.assert_not_called()
        self.assertEqual(len(printed), 1)
        self.assertTrue(printed[0][1]["flush"])
        self.assertIn("max_cycles", json.loads(printed[0][0][0])["error"])

        with self.assertRaises(ValueError):
            list(dead_air.watch_cycles(self.config))
        with self.assertRaises(ValueError):
            list(dead_air.watch_cycles(self.config, 3, continuous=True))

    def test_continuous_watch_streams_flushed_cycles_and_interrupts_cleanly(self):
        with tempfile.TemporaryDirectory() as td:
            state_path = Path(td) / "state.db"
            base = self.config
            stream = dataclasses.replace(
                base.streams[0], min_bad_samples=1, unhealthy_after_s=0, recovery_samples=1,
            )
            destination = dead_air.DestinationConfig("fake", "webhook", endpoint_env="FAKE_URL")
            watch = dataclasses.replace(base.watch, state_path=str(state_path), interval_s=30)
            config = dataclasses.replace(
                base, watch=watch, streams=(stream,), destinations=(destination,),
            )
            statuses = ["UNHEALTHY", "UNHEALTHY", "HEALTHY"]
            observed, writes, sleeps, sender_calls = [], [], [], []
            wall, monotonic = [100.0], [10.0]

            def cycle(_config):
                index = len(observed)
                status = statuses[index]
                observed.append(status)
                return (self._watch_observation(status, index),)

            def sleeper(interval_s):
                sleeps.append((interval_s, len(writes), len(observed)))
                wall[0] += interval_s
                monotonic[0] += interval_s

            def sender(_destination, event, _network):
                sender_calls.append((event["event_id"], event["transition"]))
                if len(sender_calls) == 1:
                    return False, "HTTP_STATUS", 503, None
                return True, None, 204, None

            real_watch_cycles = dead_air.watch_cycles
            real_dispatch_due = dead_air.dispatch_due

            def fake_watch(config_arg, max_cycles=None, *, continuous=False, store=None):
                return real_watch_cycles(
                    config_arg, max_cycles, continuous=continuous, cycle=cycle,
                    sleeper=sleeper, store=store,
                )

            def fake_dispatch(store, destinations, network):
                return real_dispatch_due(
                    store, destinations, network, now_wall=wall[0], sender=sender,
                    monotonic=lambda: monotonic[0],
                )

            def output(line, **kwargs):
                writes.append((line, kwargs.get("flush"), len(observed)))
                if len(writes) == 3:
                    raise KeyboardInterrupt

            with patch.object(dead_air, "load_config", return_value=config), \
                    patch.object(dead_air, "watch_cycles", side_effect=fake_watch), \
                    patch.object(dead_air, "dispatch_due", side_effect=fake_dispatch), \
                    patch.object(dead_air.time, "time", side_effect=lambda: wall[0]), \
                    patch.object(dead_air.time, "monotonic", side_effect=lambda: monotonic[0]), \
                    patch("builtins.print", side_effect=output):
                result = dead_air.main([
                    "watch", "--config", "unused.toml", "--continuous", "--json",
                ])

            self.assertEqual(result, 130)
            self.assertEqual(observed, statuses)
            self.assertEqual([count for _, _, count in writes], [1, 2, 3])
            self.assertTrue(all(flush for _, flush, _ in writes))
            self.assertEqual(sleeps, [(30, 1, 1), (30, 2, 2)])
            self.assertEqual(sender_calls[0][0], sender_calls[1][0])
            self.assertEqual([transition for _, transition in sender_calls],
                             ["open", "open", "recovery"])

            with dead_air.IncidentStore(state_path, config.retention_days) as store:
                events = store.events()
                deliveries = {row[0]: row for row in store.deliveries()}
                self.assertEqual([row[2] for row in events], ["open", "recovery"])
                opened_id, recovered_id = events[0][0], events[1][0]
                self.assertEqual([event_id for event_id, _ in sender_calls],
                                 [opened_id, opened_id, recovered_id])
                self.assertEqual(deliveries[opened_id][2:4], ("sent", 2))
                self.assertEqual(deliveries[recovered_id][2:4], ("sent", 1))
                self.assertEqual(store.open_incidents(), [])

    def test_continuous_sender_cleanup_failure_stops_and_persists_before_exit(self):
        with tempfile.TemporaryDirectory() as td:
            state_path = Path(td) / "state.db"
            base = self.config
            stream = dataclasses.replace(
                base.streams[0], min_bad_samples=1, unhealthy_after_s=0, recovery_samples=1,
            )
            destination = dead_air.DestinationConfig("fake", "webhook", endpoint_env="FAKE_URL")
            watch = dataclasses.replace(base.watch, state_path=str(state_path), interval_s=30)
            config = dataclasses.replace(
                base, watch=watch, streams=(stream,), destinations=(destination,),
            )
            statuses = ["UNHEALTHY", "HEALTHY", "HEALTHY"]
            observed, writes, sleeps, sender_calls = [], [], [], []
            wall, monotonic = [100.0], [10.0]

            def cycle(_config):
                index = len(observed)
                status = statuses[index]
                observed.append(status)
                return (self._watch_observation(status, index),)

            def sleeper(interval_s):
                sleeps.append((interval_s, len(writes), len(observed)))
                wall[0] += interval_s
                monotonic[0] += interval_s

            def sender(_destination, event, _network):
                sender_calls.append((event["event_id"], event["transition"]))
                if len(sender_calls) == 1:
                    return True, None, 204, None
                return False, "DELIVERY_CLEANUP_FAILED", None, None

            real_watch_cycles = dead_air.watch_cycles
            real_dispatch_due = dead_air.dispatch_due

            def fake_watch(config_arg, max_cycles=None, *, continuous=False, store=None):
                return real_watch_cycles(
                    config_arg, max_cycles, continuous=continuous, cycle=cycle,
                    sleeper=sleeper, store=store,
                )

            def fake_dispatch(store, destinations, network):
                return real_dispatch_due(
                    store, destinations, network, now_wall=wall[0], sender=sender,
                    monotonic=lambda: monotonic[0],
                )

            def output(line, **kwargs):
                writes.append((line, kwargs.get("flush")))

            with patch.object(dead_air, "load_config", return_value=config), \
                    patch.object(dead_air, "watch_cycles", side_effect=fake_watch), \
                    patch.object(dead_air, "dispatch_due", side_effect=fake_dispatch), \
                    patch.object(dead_air.time, "time", side_effect=lambda: wall[0]), \
                    patch.object(dead_air.time, "monotonic", side_effect=lambda: monotonic[0]), \
                    patch("builtins.print", side_effect=output):
                result = dead_air.main([
                    "watch", "--config", "unused.toml", "--continuous", "--json",
                ])

            self.assertEqual(result, 3)
            self.assertEqual(observed, statuses[:2])
            self.assertEqual(len(sender_calls), 2)
            self.assertEqual(sleeps, [(30, 1, 1)])
            self.assertEqual(len(writes), 2)
            self.assertTrue(all(flush for _, flush in writes))
            self.assertIn("observations", json.loads(writes[0][0]))
            error = json.loads(writes[1][0])
            self.assertEqual(error["error"], "DELIVERY_CLEANUP_FAILED")
            self.assertNotIn("FAKE_URL", writes[1][0])

            with dead_air.IncidentStore(state_path, config.retention_days) as store:
                events = store.events()
                deliveries = {row[0]: row for row in store.deliveries()}
                self.assertEqual([row[2] for row in events], ["open", "recovery"])
                self.assertEqual(len(sender_calls), 2)
                opened_id, failed_id = events[0][0], events[1][0]
                self.assertEqual(deliveries[opened_id][2:4], ("sent", 1))
                self.assertEqual(deliveries[failed_id][2:5],
                                 ("pending", 1, "DELIVERY_CLEANUP_FAILED"))

    def test_finite_watch_aggregates_unhealthy_over_unknown_and_healthy(self):
        scenarios = (
            (["UNKNOWN", "UNHEALTHY", "HEALTHY"], 1),
            (["UNKNOWN", "HEALTHY"], 2),
            (["HEALTHY"], 0),
        )
        for case_index, (statuses, expected) in enumerate(scenarios):
            with self.subTest(statuses=statuses), tempfile.TemporaryDirectory() as td:
                state_path = Path(td) / "state.db"
                base = self.config
                config = dataclasses.replace(
                    base, watch=dataclasses.replace(base.watch, state_path=str(state_path)),
                )
                observed, writes = [], []

                def cycle(_config):
                    index = len(observed)
                    status = statuses[index]
                    observed.append(status)
                    return (self._watch_observation(status, index),)

                real_watch_cycles = dead_air.watch_cycles

                def fake_watch(config_arg, max_cycles=None, *, continuous=False, store=None):
                    return real_watch_cycles(
                        config_arg, max_cycles, continuous=continuous, cycle=cycle,
                        sleeper=lambda _seconds: None, store=store,
                    )

                with patch.object(dead_air, "load_config", return_value=config), \
                        patch.object(dead_air, "watch_cycles", side_effect=fake_watch), \
                        patch("builtins.print",
                              side_effect=lambda line, **kwargs: writes.append((line, kwargs.get("flush")))):
                    result = dead_air.main([
                        "watch", "--config", f"unused-{case_index}.toml",
                        "--max-cycles", str(len(statuses)), "--json",
                    ])
                self.assertEqual(result, expected)
                self.assertEqual(observed, statuses)
                self.assertEqual(len(writes), len(statuses))
                self.assertTrue(all(flush for _, flush in writes))

    def test_doctor_uses_fake_local_binary_checks_and_no_network(self):
        def runner(argv, timeout, **kwargs):
            if "--help" in argv:
                text = " ".join(dead_air.YTDLP_FLAGS)
            elif "-filters" in argv:
                text = "freezedetect silencedetect"
            else:
                text = "tool version fixture"
            return dead_air.ChildResult(text, "", 0)
        with patch.object(dead_air.shutil, "which", return_value="fake-tool"):
            result = dead_air.doctor(self.config, runner=runner)
        self.assertTrue(result["ok"])
        self.assertFalse(result["network_contacted"])
        self.assertTrue(result["tools"]["resolver_isolation_flags_available"])


if __name__ == "__main__":
    unittest.main()
