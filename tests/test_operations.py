"""Operator-facing regressions; no public network or real notification targets."""

import dataclasses
import json
import os
import subprocess
import sys
import tempfile
import tomllib
import unittest
from pathlib import Path
from unittest.mock import patch

import dead_air
from test_dead_air import EXAMPLE, test_config


def local_runner(argv, _timeout, **_kwargs):
    if "--help" in argv:
        output = " ".join(dead_air.YTDLP_FLAGS)
    elif "-filters" in argv:
        output = "freezedetect silencedetect"
    else:
        output = "fixture version 1"
    return dead_air.ChildResult(output, "", 0)


class ConfigurationOperationsTests(unittest.TestCase):
    def test_direct_doctor_does_not_require_optional_youtube_tool(self):
        config = test_config()
        with patch.object(dead_air.shutil, "which", side_effect=lambda tool: None if tool == config.tools.yt_dlp else tool):
            result = dead_air.doctor(config, runner=local_runner)
        self.assertTrue(result["ok"])
        self.assertFalse(result["tools"]["yt_dlp"]["required"])
        youtube = dataclasses.replace(config.streams[0], source="youtube")
        with patch.object(dead_air.shutil, "which", side_effect=lambda tool: None if tool == config.tools.yt_dlp else tool):
            result = dead_air.doctor(dataclasses.replace(config, streams=(youtube,)), runner=local_runner)
        self.assertFalse(result["ok"])

    def test_doctor_rejects_truncated_or_unreaped_filter_check(self):
        for failure in ({"timed_out": True}, {"output_limited": True}, {"cleanup_complete": False}):
            with self.subTest(failure=failure):
                def runner(argv, timeout, **kwargs):
                    result = local_runner(argv, timeout, **kwargs)
                    return dataclasses.replace(result, **failure) if "-filters" in argv else result
                with patch.object(dead_air.shutil, "which", side_effect=lambda x: x):
                    self.assertFalse(dead_air.doctor(test_config(), runner=runner)["ok"])

    def test_paths_resolve_beside_config_but_bare_tool_names_stay_on_path(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = root / "custom.toml"
            text = EXAMPLE.read_text().replace('ffmpeg = "ffmpeg"', 'ffmpeg = "./bin/ffmpeg"')
            path.write_text(text)
            config = dead_air.load_config(path)
            self.assertEqual(config.watch.state_path, str(root / "state/dead-air.sqlite3"))
            self.assertEqual(config.tools.ffmpeg, str(root / "bin/ffmpeg"))
            self.assertEqual(config.tools.ffprobe, "ffprobe")
            self.assertFalse((root / "state").exists())

    def test_malformed_destination_types_are_config_errors(self):
        for kind in ([], {}, 5):
            raw = tomllib.loads(EXAMPLE.read_text())
            raw["destinations"] = [{"id": "bad", "type": kind}]
            with self.subTest(kind=kind), self.assertRaises(dead_air.ConfigError):
                dead_air.validate_config(raw)

    def test_invalid_utf8_is_bounded_config_error(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "bad.toml"
            path.write_bytes(b"\xff\xfe")
            with self.assertRaises(dead_air.ConfigError):
                dead_air.load_config(path)

    def test_explicit_zero_port_is_rejected(self):
        with self.assertRaises(dead_air.ConfigError):
            dead_air._static_url("https://example.invalid:0/live", "direct")


class SchedulingOperationsTests(unittest.TestCase):
    def test_each_stream_gets_a_share_of_the_remaining_deadline(self):
        base = test_config()
        streams = tuple(dataclasses.replace(base.streams[0], id=f"stream-{i}") for i in range(3))
        config = dataclasses.replace(base, streams=streams,
                                     watch=dataclasses.replace(base.watch, cycle_budget_s=30))
        now = [100.0]
        deadlines = []

        def observe(_config, stream, **kwargs):
            deadline = kwargs["deadline"]
            deadlines.append(deadline)
            now[0] = deadline
            return dead_air._unknown_observation(stream, "FIXTURE_TIMEOUT")

        with patch.object(dead_air, "observe_stream", side_effect=observe):
            result = dead_air.check_cycle(config, clock=lambda: now[0])
        self.assertEqual(deadlines, [110, 120, 130])
        self.assertEqual([item.stream_id for item in result], [item.id for item in streams])

    def test_watch_interval_is_start_to_start_and_overruns_do_not_catch_up(self):
        for cycle_s, expected_sleep in ((7, 23), (40, 0)):
            with self.subTest(cycle_s=cycle_s):
                config = test_config()
                config = dataclasses.replace(config, watch=dataclasses.replace(config.watch, interval_s=30))
                now = [100.0]
                starts, sleeps = [], []
                def cycle(_config):
                    starts.append(now[0])
                    now[0] += cycle_s
                    return ()
                def sleep(seconds):
                    sleeps.append(seconds)
                    now[0] += seconds
                list(dead_air.watch_cycles(config, 3, cycle=cycle, sleeper=sleep, clock=lambda: now[0]))
                self.assertEqual(sleeps, [expected_sleep, expected_sleep])
                self.assertEqual(starts, [100, 100 + max(30, cycle_s), 100 + 2 * max(30, cycle_s)])

    def test_missing_track_recovers_into_silent_or_frozen_media(self):
        from test_incidents import config, observation, silence, freeze
        cfg = config(min_bad=1, unhealthy=0, recovery=1)
        missing = observation("UNKNOWN", audio=dead_air.DimensionEvidence("UNKNOWN", ("AUDIO_MISSING",)),
                              video=dead_air.DimensionEvidence("UNKNOWN", ("VIDEO_MISSING",)))
        both = observation("UNHEALTHY", audio=silence(), video=freeze(),
                           reasons=("AUDIO_SILENCE", "VIDEO_FREEZE"))
        with tempfile.TemporaryDirectory() as directory, dead_air.IncidentStore(Path(directory) / "state.db") as store:
            store.record_cycle(cfg, (missing,), 100, 0)
            self.assertEqual({row[1] for row in store.open_incidents()}, {"audio_missing", "video_missing"})
            store.record_cycle(cfg, (both,), 101, 1)
            self.assertEqual({row[1] for row in store.open_incidents()}, {"audio_silence", "video_freeze"})


if __name__ == "__main__":
    unittest.main()
