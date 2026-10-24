"""Operator-facing regressions; no public network or real notification targets."""

import dataclasses
import io
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


class PersistenceOperationsTests(unittest.TestCase):
    def test_one_writer_per_database_and_separate_databases_share_a_directory(self):
        with tempfile.TemporaryDirectory() as directory:
            first, second = Path(directory) / "first.db", Path(directory) / "second.db"
            with dead_air.IncidentStore(first):
                with self.assertRaisesRegex(dead_air.PersistenceError, "STATE_LOCKED"):
                    dead_air.IncidentStore(first)
                with dead_air.IncidentStore(second):
                    self.assertTrue(second.exists())

    def test_lock_is_released_by_os_after_abrupt_process_exit(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "state.db"
            code = "import os,sys; from dead_air import IncidentStore; s=IncidentStore(sys.argv[1]); os._exit(0)"
            result = subprocess.run([sys.executable, "-c", code, str(path)], timeout=5, capture_output=True)
            self.assertEqual(result.returncode, 0, result.stderr.decode())
            with dead_air.IncidentStore(path) as store:
                self.assertEqual(store.open_incidents(), [])

    def test_read_only_status_during_watch_preserves_counters_and_reports_deliveries(self):
        from deadair.status import read_status
        from test_incidents import config, observation, silence
        destination = dead_air.DestinationConfig("ops", "webhook", endpoint_env="FIXTURE_URL")
        cfg = config(min_bad=1, unhealthy=0, recovery=2, destinations=(destination,))
        bad = observation(audio=silence(), reasons=("AUDIO_SILENCE",))
        good = observation("HEALTHY")
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "state.db"
            cfg = dataclasses.replace(cfg, watch=dataclasses.replace(cfg.watch, state_path=str(path)))
            with dead_air.IncidentStore(path) as store:
                store.record_cycle(cfg, (bad,), 100, 0)
                store.record_cycle(cfg, (good,), 101, 1)
                snapshot = read_status(cfg, now_wall=102)
                self.assertEqual(snapshot["status"], "UNHEALTHY")
                self.assertEqual(snapshot["streams"][0]["observation_status"], "HEALTHY")
                self.assertEqual(snapshot["deliveries"]["pending"], 1)
                self.assertEqual(snapshot["open_incidents"][0]["condition"], "audio_silence")
                self.assertNotIn("https://", json.dumps(snapshot))
                self.assertNotIn("FIXTURE_URL", json.dumps(snapshot))
                store.record_cycle(cfg, (good,), 102, 2)
                self.assertEqual(store.open_incidents(), [])
            with dead_air.IncidentStore(path):
                snapshot = read_status(cfg, now_wall=103)
                self.assertEqual(snapshot["streams"][0]["observation_status"], "HEALTHY")

    def test_status_marks_absent_stale_rollback_and_changed_sources_unknown(self):
        from deadair.status import read_status
        from test_incidents import observation
        with tempfile.TemporaryDirectory() as directory:
            cfg = test_config()
            path = Path(directory) / "not-created" / "state.db"
            cfg = dataclasses.replace(cfg, watch=dataclasses.replace(cfg.watch, state_path=str(path)))
            self.assertEqual(read_status(cfg, now_wall=100)["status"], "UNKNOWN")
            self.assertFalse(path.parent.exists())
            with dead_air.IncidentStore(path) as store:
                store.record_cycle(cfg, (observation("HEALTHY"),), 100, 0)
                self.assertEqual(read_status(cfg, now_wall=101)["status"], "HEALTHY")
                self.assertEqual(read_status(cfg, now_wall=100 + cfg.streams[0].max_gap_s + 1)["status"], "UNKNOWN")
                self.assertEqual(read_status(cfg, now_wall=99)["status"], "UNKNOWN")
                changed = dataclasses.replace(cfg, streams=(dataclasses.replace(cfg.streams[0], url="https://example.invalid/changed"),))
                self.assertEqual(read_status(changed, now_wall=101)["status"], "UNKNOWN")

    def test_legacy_lock_is_never_removed_or_bypassed(self):
        with tempfile.TemporaryDirectory() as directory:
            legacy = Path(directory) / ".dead-air.lock"
            legacy.write_text("unknown owner")
            with self.assertRaises(dead_air.PersistenceError):
                dead_air.IncidentStore(Path(directory) / "state.db")
            self.assertEqual(legacy.read_text(), "unknown owner")

    def test_check_has_human_output_and_json_contains_alert_diagnostics(self):
        from test_incidents import observation
        with tempfile.TemporaryDirectory() as directory:
            config = test_config()
            config = dataclasses.replace(config, watch=dataclasses.replace(config.watch, state_path=str(Path(directory) / "state.db")))
            for json_mode in (False, True):
                output = io.StringIO()
                with patch.object(dead_air, "load_config", return_value=config), \
                        patch.object(dead_air, "check_cycle", return_value=(observation("HEALTHY"),)), \
                        patch("sys.stdout", output):
                    code = dead_air.main(["check"] + (["--json"] if json_mode else []))
                self.assertEqual(code, 0)
                if json_mode:
                    result = json.loads(output.getvalue())
                    self.assertIn("observations", result)
                    self.assertIn("deliveries", result)
                else:
                    self.assertIn("sample-channel", output.getvalue())
                    self.assertIn("HEALTHY", output.getvalue())
                    self.assertFalse(output.getvalue().startswith("{"))


class LifecycleOperationsTests(unittest.TestCase):
    @unittest.skipUnless(sys.platform == "linux", "Linux process-state inspection")
    def test_exited_parent_cannot_leave_an_owned_descendant_holding_pipes(self):
        import signal
        code = ("import subprocess,sys; child=subprocess.Popen([sys.executable,'-c',"
                "'import signal,time; signal.signal(signal.SIGTERM,signal.SIG_IGN); time.sleep(30)']); "
                "print(child.pid,flush=True)")
        result = dead_air.run_bounded([sys.executable, "-c", code], 2, max_output_bytes=256)
        pid = int(result.stdout.strip())
        try:
            process = Path(f"/proc/{pid}/stat")
            state = process.read_text().rsplit(")", 1)[1].split()[0] if process.exists() else None
            self.assertIn(state, (None, "Z"), "owned descendant remained running")
            self.assertTrue(result.cleanup_complete)
        finally:
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass

    def test_init_creates_valid_config_without_overwriting_an_existing_file(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "private.toml"
            with patch("sys.stdout", io.StringIO()):
                self.assertEqual(dead_air.main(["init", "--config", str(path)]), 0)
            self.assertEqual(len(dead_air.load_config(path).streams), 1)
            original = path.read_bytes()
            with patch("sys.stdout", io.StringIO()):
                self.assertEqual(dead_air.main(["init", "--config", str(path)]), 3)
            self.assertEqual(path.read_bytes(), original)

    @unittest.skipUnless(os.name == "posix", "POSIX service signal regression")
    def test_sigterm_cleans_owned_state_and_restores_previous_handler(self):
        import signal
        config = test_config()
        previous = signal.getsignal(signal.SIGTERM)
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "state.db"
            config = dataclasses.replace(config, watch=dataclasses.replace(config.watch, state_path=str(path)))
            def interrupted(_config, **_kwargs):
                os.kill(os.getpid(), signal.SIGTERM)
                return ()
            with patch.object(dead_air, "load_config", return_value=config), \
                    patch.object(dead_air, "check_cycle", side_effect=interrupted):
                self.assertEqual(dead_air.main(["check", "--json"]), 143)
            self.assertIs(signal.getsignal(signal.SIGTERM), previous)
            with dead_air.IncidentStore(path):
                pass

    def test_package_modules_import_independently(self):
        for name in ("deadair.persistence", "deadair.config", "deadair.status"):
            result = subprocess.run([sys.executable, "-c", f"import {name}"], capture_output=True, timeout=5)
            self.assertEqual(result.returncode, 0, result.stderr.decode())


if __name__ == "__main__":
    unittest.main()
