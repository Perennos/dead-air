"""Required native media acceptance. This suite is deliberately not run by Slice B."""

import dataclasses
import shutil
import tempfile
import unittest
from pathlib import Path

import dead_air


class NativeMediaTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.ffmpeg = shutil.which("ffmpeg")
        cls.ffprobe = shutil.which("ffprobe")
        if not cls.ffmpeg or not cls.ffprobe:
            raise RuntimeError("required native media dependency missing: ffmpeg and ffprobe are mandatory")
        cls.directory = tempfile.TemporaryDirectory(prefix="dead-air-media-acceptance-")
        cls.addClassCleanup(cls.directory.cleanup)
        cls.root = Path(cls.directory.name)
        base = dead_air.load_config(Path(__file__).parents[1] / "dead-air.example.toml")
        watch = dataclasses.replace(
            base.watch, sample_s=3, capture_timeout_s=15, analysis_timeout_s=15,
            max_sample_bytes=16_000_000, max_output_bytes=65_536,
        )
        tools = dataclasses.replace(base.tools, ffmpeg=cls.ffmpeg, ffprobe=cls.ffprobe)
        stream = dataclasses.replace(
            base.streams[0], freeze_window_s=1, silence_window_s=1,
            freeze_noise=0.001, silence_db=-50,
        )
        cls.config = dataclasses.replace(base, watch=watch, tools=tools, streams=(stream,))
        cls.stream = stream
        cls.fixtures = {
            "normal": cls._generate("normal", "testsrc2=size=160x90:rate=10", "sine=frequency=1000:sample_rate=48000", 3),
            "frozen": cls._generate("frozen", "color=c=black:size=160x90:rate=10", "sine=frequency=1000:sample_rate=48000", 3),
            "silent": cls._generate("silent", "testsrc2=size=160x90:rate=10", "anullsrc=r=48000:cl=stereo", 3),
            "frozen_silent": cls._generate("frozen-silent", "color=c=black:size=160x90:rate=10", "anullsrc=r=48000:cl=stereo", 3),
            "no_audio": cls._generate("no-audio", "testsrc2=size=160x90:rate=10", None, 3),
            "audio_only": cls._generate("audio-only", None, "sine=frequency=1000:sample_rate=48000", 3),
            "short": cls._generate("short", "testsrc2=size=160x90:rate=10", "sine=frequency=1000:sample_rate=48000", 0.5),
            "long_video_short_audio": cls._generate(
                "long-video-short-audio", "testsrc2=size=160x90:rate=10",
                "sine=frequency=1000:sample_rate=48000:duration=0.1", 12,
            ),
            "long_video_short_silence": cls._generate(
                "long-video-short-silence", "testsrc2=size=160x90:rate=10",
                "anullsrc=r=48000:cl=stereo:d=0.1", 12,
            ),
            "long_audio_one_frame_video": cls._generate(
                "long-audio-one-frame-video", "color=c=black:size=160x90:rate=10:d=0.1",
                "sine=frequency=1000:sample_rate=48000", 12,
            ),
        }
        cls.fixtures["corrupt"] = cls.root / "corrupt.mkv"
        cls.fixtures["corrupt"].write_bytes(b"not a media container")

    @classmethod
    def _generate(cls, name, video_source, audio_source, duration):
        target = cls.root / f"{name}.mkv"
        argv = [
            cls.ffmpeg, "-nostdin", "-hide_banner", "-loglevel", "error", "-y",
            "-threads", "1", "-filter_threads", "1",
        ]
        video_index = None
        audio_index = None
        input_index = 0
        if video_source:
            argv.extend(["-f", "lavfi", "-i", video_source])
            video_index = input_index
            input_index += 1
        if audio_source:
            argv.extend(["-f", "lavfi", "-i", audio_source])
            audio_index = input_index
        argv.extend(["-t", str(duration)])
        if video_index is not None:
            argv.extend(["-map", f"{video_index}:v:0", "-c:v", "ffv1"])
        if audio_source:
            argv.extend(["-map", f"{audio_index}:a:0", "-c:a", "pcm_s16le"])
        if not audio_source:
            argv.append("-an")
        argv.append(str(target))
        result = dead_air.run_bounded(
            argv, 15, max_output_bytes=8192,
            file_limits=((target, 16_000_000),),
        )
        if result.returncode != 0 or result.timed_out or result.output_limited or result.file_limited or not result.cleanup_complete:
            raise RuntimeError(f"lavfi fixture generation failed for {name}: {result.returncode}")
        if not target.is_file() or not 0 < target.stat().st_size < 16_000_000:
            raise RuntimeError(f"lavfi fixture invalid for {name}")
        return target

    def inspect(self, name, stream=None):
        current = stream or self.stream
        config = dataclasses.replace(self.config, streams=(current,))
        return dead_air.inspect_sample(self.fixtures[name], current, config)

    def test_normal_audio_video_is_healthy(self):
        result = self.inspect("normal")
        self.assertEqual((result.status, result.audio.status, result.video.status), ("HEALTHY", "HEALTHY", "HEALTHY"))

    def test_frozen_picture_with_tone_only_fails_video_dimension(self):
        result = self.inspect("frozen")
        self.assertEqual(result.video.status, "UNHEALTHY")
        self.assertEqual(result.video.reason_codes, ("VIDEO_FREEZE",))
        self.assertEqual(result.audio.status, "HEALTHY")
        self.assertNotEqual(result.status, "OFF_AIR")

    def test_moving_picture_with_silence_only_fails_audio_dimension(self):
        result = self.inspect("silent")
        self.assertEqual(result.audio.status, "UNHEALTHY")
        self.assertEqual(result.audio.reason_codes, ("AUDIO_SILENCE",))
        self.assertEqual(result.video.status, "HEALTHY")

    def test_frozen_and_silent_reports_both_dimensions(self):
        result = self.inspect("frozen_silent")
        self.assertEqual(result.audio.status, "UNHEALTHY")
        self.assertEqual(result.video.status, "UNHEALTHY")
        self.assertEqual(set(result.reason_codes), {"AUDIO_SILENCE", "VIDEO_FREEZE"})

    def test_missing_required_audio_is_unknown_not_silence(self):
        result = self.inspect("no_audio")
        self.assertEqual(result.audio.status, "UNKNOWN")
        self.assertEqual(result.audio.reason_codes, ("AUDIO_MISSING",))
        self.assertEqual(result.video.status, "HEALTHY")

    def test_configured_audio_only_fixture_is_supported(self):
        stream = dataclasses.replace(self.stream, expect_video=False, detect_freeze=False)
        result = self.inspect("audio_only", stream)
        self.assertEqual((result.status, result.audio.status, result.video.status),
                         ("HEALTHY", "HEALTHY", "NOT_APPLICABLE"))

    def test_configured_video_only_fixture_is_supported(self):
        stream = dataclasses.replace(self.stream, expect_audio=False)
        result = self.inspect("no_audio", stream)
        self.assertEqual((result.status, result.audio.status, result.video.status),
                         ("HEALTHY", "NOT_APPLICABLE", "HEALTHY"))

    def test_short_capture_is_unknown(self):
        result = self.inspect("short")
        self.assertEqual(result.audio.status, "UNKNOWN")
        self.assertEqual(result.video.status, "UNKNOWN")

    def test_short_audible_audio_cannot_borrow_long_video_coverage(self):
        result = self.inspect("long_video_short_audio")
        self.assertEqual(result.audio.status, "UNKNOWN")
        self.assertEqual(result.video.status, "HEALTHY")
        self.assertLess(result.audio.decoded_s, self.config.watch.sample_s)
        self.assertGreaterEqual(result.video.decoded_s, self.config.watch.sample_s - 0.1)

    def test_short_silent_audio_interval_cannot_borrow_long_video_coverage(self):
        result = self.inspect("long_video_short_silence")
        self.assertEqual(result.audio.status, "UNKNOWN")
        self.assertNotEqual(result.audio.status, "UNHEALTHY")
        self.assertEqual(result.video.status, "HEALTHY")
        self.assertLess(result.audio.decoded_s, self.config.watch.sample_s)

    def test_one_frozen_video_frame_cannot_borrow_long_audio_coverage(self):
        result = self.inspect("long_audio_one_frame_video")
        self.assertEqual(result.video.status, "UNKNOWN")
        self.assertEqual(result.audio.status, "HEALTHY")
        self.assertLess(result.video.decoded_s, self.config.watch.sample_s)
        self.assertGreaterEqual(result.audio.decoded_s, self.config.watch.sample_s - 0.1)

    def test_delayed_short_audio_does_not_certify_full_sample_window(self):
        output = self.root / "delayed-short-audio.mkv"
        result = dead_air.run_bounded([
            self.ffmpeg, "-nostdin", "-hide_banner", "-loglevel", "error", "-y",
            "-threads", "1", "-filter_threads", "1", "-filter_complex_threads", "1",
            "-f", "lavfi", "-i", "testsrc2=size=160x90:rate=10:duration=3",
            "-f", "lavfi", "-i", "sine=frequency=1000:sample_rate=48000:duration=0.1",
            "-filter_complex", "[1:a]asetpts=PTS+2.9/TB[a]",
            "-map", "0:v:0", "-map", "[a]", "-c:v", "ffv1", "-c:a", "pcm_s16le",
            "-t", "3", str(output),
        ], 15, max_output_bytes=8192, file_limits=((output, 16_000_000),))
        self.assertEqual(result.returncode, 0)
        self.assertTrue(result.cleanup_complete)
        observed = self.inspect_path(output)
        self.assertEqual(observed.audio.status, "UNKNOWN")
        self.assertAlmostEqual(observed.audio.decoded_s, 0.1, delta=0.02)
        self.assertEqual(observed.video.status, "HEALTHY")
        self.assertAlmostEqual(observed.video.decoded_s, 3.0, delta=0.02)

    def inspect_path(self, path, stream=None):
        current = stream or self.stream
        config = dataclasses.replace(self.config, streams=(current,))
        return dead_air.inspect_sample(path, current, config)

    def test_corrupt_capture_is_unknown(self):
        result = self.inspect("corrupt")
        self.assertEqual(result.status, "UNKNOWN")


if __name__ == "__main__":
    unittest.main()
