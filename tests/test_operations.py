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


if __name__ == "__main__":
    unittest.main()
