import contextlib
import dataclasses
import hashlib
import http.client
import io
import json
import os
import re
import shutil
import signal
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path
from unittest.mock import patch

import dead_air
import dead_air_gui
from test_dead_air import EXAMPLE
from test_incidents import config as incident_config, observation, silence


ROOT = Path(__file__).parents[1]
DESTINATIONS = """
[[destinations]]
id = "ops-ntfy"
type = "ntfy"
endpoint_env = "DEAD_AIR_TEST_NTFY_URL"

[[destinations]]
id = "ops-discord"
type = "discord"
endpoint_env = "DEAD_AIR_TEST_DISCORD_URL"
"""


class GuiTestCase(unittest.TestCase):
    """A real server on 127.0.0.1:0 over a temp config copied from the example. Nothing leaves the machine."""

    destinations = False
    create_config = True

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.dir = Path(self.temp.name)
        self.state = self.dir / "state" / "dead-air.sqlite3"
        self.config = self.dir / "dead-air.toml"
        if self.create_config:
            text = EXAMPLE.read_text(encoding="utf-8").replace(
                'state_path = "state/dead-air.sqlite3"', f'state_path = "{self.state.as_posix()}"')
            if self.destinations:
                text += DESTINATIONS
            self.config.write_text(text, encoding="utf-8")
        self.server = dead_air_gui.make_server(self.config, 0)
        self.assertEqual(self.server.server_address[0], "127.0.0.1")
        self.assertTrue(self.server.daemon_threads)
        self.port = self.server.server_port
        thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        thread.start()
        self.addCleanup(thread.join, 5)
        self.addCleanup(self.server.server_close)
        self.addCleanup(self.server.shutdown)
        self.addCleanup(dead_air_gui.stop_watch, self.server)

    def request(self, method, path, body=None, headers=None):
        connection = http.client.HTTPConnection("127.0.0.1", self.port, timeout=10)
        try:
            connection.request(method, path, body=body, headers=headers or {})
            response = connection.getresponse()
            return response.status, response, response.read()
        finally:
            connection.close()

    def get_json(self, path="/api/state"):
        status, _, body = self.request("GET", path)
        self.assertEqual(status, 200)
        return json.loads(body)

    def post(self, path, payload, *, token=True, headers=None):
        merged = {"Content-Type": "application/json"}
        if token:
            merged["X-Token"] = self.server.token
        merged.update(headers or {})
        body = payload if isinstance(payload, bytes) else json.dumps(payload).encode("utf-8")
        status, response, data = self.request("POST", path, body, merged)
        return status, json.loads(data) if data else None

    def add(self, name="Second stream", url="https://example.invalid/live/second", source="direct", **kwargs):
        return self.post("/api/streams", {"name": name, "url": url, "source": source}, **kwargs)


class PageAndHeaderTests(GuiTestCase):
    def test_index_carries_the_per_run_token_and_no_inline_script_or_style(self):
        status, response, body = self.request("GET", "/")
        text = body.decode("utf-8")
        self.assertEqual(status, 200)
        self.assertIn("text/html", response.getheader("Content-Type"))
        self.assertEqual(re.search(r'<meta name="x-token" content="([^"]+)">', text).group(1), self.server.token)
        self.assertGreaterEqual(len(self.server.token), 43)
        self.assertNotIn("__TOKEN__", text)
        self.assertNotRegex(text, r"<script(?![^>]*\ssrc=)")
        self.assertNotIn("<style", text)
        self.assertNotRegex(text, r"\sstyle\s*=")
        self.assertNotRegex(text, r"\bon[a-z]+\s*=")

    def test_assets_are_served_and_the_stamp_leads_the_stylesheet(self):
        status, response, css = self.request("GET", "/app.css")
        self.assertEqual(status, 200)
        self.assertTrue(css.decode("utf-8").startswith("/* Hallmark · component: local GUI · genre: atmospheric"))
        status, response, js = self.request("GET", "/app.js")
        self.assertEqual(status, 200)
        self.assertIn("javascript", response.getheader("Content-Type"))
        self.assertIn(b"/api/state", js)

    def test_brand_files_are_byte_equal_and_nothing_else_is_reachable(self):
        for name in ("mark.svg", "mark-dead.svg", "favicon.svg"):
            status, response, body = self.request("GET", f"/brand/{name}")
            self.assertEqual(status, 200)
            self.assertEqual(response.getheader("Content-Type"), "image/svg+xml")
            self.assertEqual(body, (ROOT / "brand" / name).read_bytes())
        for path in ("/brand/mark-light.svg", "/brand/NOTES.md", "/brand/../dead_air.py",
                     "/brand/%2e%2e/dead_air.py", "/dead_air.py", "/gui/app.js", "/api/streams", "/nope"):
            self.assertEqual(self.request("GET", path)[0], 404, path)

    def test_page_files_have_no_standalone_ai_word_and_no_emoji(self):
        for name in ("index.html", "app.css", "app.js"):
            text = (ROOT / "gui" / name).read_text(encoding="utf-8")
            self.assertIsNone(re.search(r"\bAI\b", text), name)
            self.assertIsNone(re.search("[\U0001F000-\U0001FFFF\u2600-\u27BF]", text), name)

    def test_security_headers_on_every_kind_of_response(self):
        wanted = {
            "Content-Security-Policy": "default-src 'self'; img-src 'self' data:; frame-ancestors 'none'; "
                                      "form-action 'self'; base-uri 'none'",
            "X-Frame-Options": "DENY",
            "X-Content-Type-Options": "nosniff",
            "Referrer-Policy": "no-referrer",
            "Cache-Control": "no-store",
        }
        responses = [
            self.request("GET", "/")[1], self.request("GET", "/app.css")[1], self.request("GET", "/app.js")[1],
            self.request("GET", "/brand/mark.svg")[1], self.request("GET", "/api/state")[1],
            self.request("GET", "/missing")[1], self.request("GET", "/", headers={"Host": "evil.example"})[1],
            self.request("POST", "/api/streams", b"{}", {"Content-Type": "application/json"})[1],
            self.request("PUT", "/api/streams")[1],
        ]
        for response in responses:
            for name, value in wanted.items():
                self.assertEqual(response.getheader(name), value, (response.status, name))


class StateTests(GuiTestCase):
    destinations = True

    def test_state_shape_before_any_check(self):
        state = self.get_json()
        self.assertEqual(state["stream_count"], 1)
        self.assertEqual(state["config_path"], str(self.config.absolute()))
        self.assertFalse(state["config_missing"])
        self.assertIsNone(state["config_error"])
        self.assertIsNone(state["state_error"])
        self.assertIsNone(state["last_cycle"])
        self.assertFalse(state["watching"])
        self.assertEqual(state["destinations"], [{"id": "ops-ntfy", "type": "ntfy"},
                                                 {"id": "ops-discord", "type": "discord"}])
        stream = state["streams"][0]
        self.assertEqual(stream, {
            "id": "sample-channel", "name": "Sample channel", "host": "example.invalid", "source": "direct",
            "status": "UNKNOWN", "severity": "uncertain", "confirming": False, "incidents": [],
            "last_alert": None, "last_checked": None,
        })

    def test_page_data_never_carries_full_urls_endpoints_or_secrets(self):
        text = self.request("GET", "/api/state")[2].decode("utf-8")
        self.assertIn("example.invalid", text)
        for forbidden in ("/live/sample", "https://", "DEAD_AIR_TEST_NTFY_URL", "endpoint", "token_env"):
            self.assertNotIn(forbidden, text)

    def test_open_incident_last_alert_and_cycle_time_come_from_the_state_file(self):
        recorder = dataclasses.replace(
            incident_config(min_bad=1, unhealthy=0),
            destinations=(dead_air.DestinationConfig("ops-ntfy", "ntfy", endpoint_env="DEAD_AIR_TEST_NTFY_URL"),))
        now = time.time()
        with dead_air.IncidentStore(self.state) as store:  # open, as a running watcher would hold it
            store.record_cycle(recorder, (observation(audio=silence(), reasons=("AUDIO_SILENCE",)),), now, 0)
            event_id = store.deliveries()[0][0]
            store.record_delivery(event_id, "ops-ntfy", True, None, 204, now, now, 0)
            before = hashlib.sha256(self.state.read_bytes()).hexdigest()
            stream = self.get_json()["streams"][0]
            state = self.get_json()
            self.assertEqual(hashlib.sha256(self.state.read_bytes()).hexdigest(), before)
        self.assertEqual((stream["status"], stream["severity"], stream["confirming"]), ("SILENT", "incident", False))
        self.assertEqual(stream["incidents"], [{"kind": "silent", "since": "2026-10-23T00:00:00Z"}])
        self.assertEqual(stream["last_alert"]["destination"], "ops-ntfy")
        self.assertEqual(stream["last_alert"]["outcome"], "sent")
        self.assertRegex(stream["last_alert"]["at"], r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ$")
        self.assertRegex(stream["last_checked"], r"Z$")
        self.assertEqual(state["last_cycle"], stream["last_checked"])
        self.assertTrue(state["watching"])
        self.get_json()  # with the writer gone: still no lock file and no change from the page
        self.assertFalse((self.state.parent / ".dead-air.lock").exists())
        self.assertEqual(hashlib.sha256(self.state.read_bytes()).hexdigest(), before)

    def test_healthy_after_a_clean_cycle_and_confirming_while_a_bad_streak_builds(self):
        slow = incident_config(min_bad=3, unhealthy=100)
        with dead_air.IncidentStore(self.state) as store:
            store.record_cycle(slow, (observation("HEALTHY"),), time.time(), 0)
            healthy = self.get_json()["streams"][0]
            store.record_cycle(slow, (observation(audio=silence(), reasons=("AUDIO_SILENCE",)),), time.time(), 1)
            confirming = self.get_json()["streams"][0]
        self.assertEqual((healthy["status"], healthy["severity"], healthy["incidents"]), ("HEALTHY", "ok", []))
        self.assertEqual((confirming["status"], confirming["confirming"], confirming["incidents"]),
                         ("SILENT", True, []))

    def test_an_old_cycle_is_not_called_watching(self):
        with dead_air.IncidentStore(self.state) as store:
            store.record_cycle(incident_config(), (observation("HEALTHY"),), time.time() - 86400, 0)
        state = self.get_json()
        self.assertFalse(state["watching"])
        self.assertEqual(state["streams"][0]["status"], "HEALTHY")

    def test_unreadable_or_foreign_state_is_reported_not_guessed(self):
        self.state.parent.mkdir(parents=True)
        self.state.write_bytes(b"this is not a database" * 100)
        state = self.get_json()
        self.assertEqual(state["state_error"], "STATE_UNREADABLE")
        self.assertEqual(state["streams"][0]["status"], "UNKNOWN")
        self.state.unlink()
        db = sqlite3.connect(self.state)
        db.execute("PRAGMA user_version=99")
        db.execute("CREATE TABLE t(x)")
        db.commit()
        db.close()
        self.assertEqual(self.get_json()["state_error"], "STATE_SCHEMA_UNSUPPORTED")

    def test_a_broken_config_is_shown_with_the_loaders_message(self):
        self.config.write_text("[watch]\n", encoding="utf-8")
        state = self.get_json()
        self.assertIn("missing", state["config_error"])
        self.assertEqual(state["streams"], [])


class TokenHostAndOriginTests(GuiTestCase):
    def test_post_without_or_with_the_wrong_token_is_403_and_changes_nothing(self):
        before = self.config.read_bytes()
        self.assertEqual(self.add(token=False)[0], 403)
        self.assertEqual(self.add(headers={"X-Token": "wrong"})[0], 403)
        self.assertEqual(self.post("/api/test-alerts", {}, token=False)[0], 403)
        self.assertEqual(self.config.read_bytes(), before)

    def test_unexpected_host_is_421_on_get_and_post(self):
        for host in ("evil.example", "evil.example:%d" % self.port, "127.0.0.1:1", "localhost.evil.example", ""):
            self.assertEqual(self.request("GET", "/api/state", headers={"Host": host})[0], 421, host)
            self.assertEqual(self.add(headers={"Host": host})[0], 421, host)
        self.assertEqual(self.request("GET", "/api/state", headers={"Host": f"localhost:{self.port}"})[0], 200)
        self.assertEqual(self.request("GET", "/api/state", headers={"Host": "LOCALHOST"})[0], 200)

    def test_a_foreign_origin_is_403_even_with_a_valid_token(self):
        before = self.config.read_bytes()
        for origin in ("https://evil.example", "http://127.0.0.1:1", "null", f"http://evil.example:{self.port}"):
            self.assertEqual(self.add(headers={"Origin": origin})[0], 403, origin)
        self.assertEqual(self.config.read_bytes(), before)
        self.assertEqual(self.add(headers={"Origin": f"http://127.0.0.1:{self.port}"})[0], 200)

    def test_body_over_64_kib_is_413_and_exactly_64_kib_is_read(self):
        self.assertEqual(self.post("/api/streams", b"x" * (64 * 1024 + 1))[0], 413)
        self.assertEqual(self.post("/api/streams", b" " * (64 * 1024 - 2) + b"[]")[0], 400)  # read, then rejected as not an object
        self.assertEqual(self.post("/api/streams", b"x" * (64 * 1024 + 1), token=False)[0], 403)

    def test_malformed_bodies_and_unknown_posts(self):
        for body in (b"not json", b"[]", b'"text"', b"\xff\xfe", b"{}"):
            self.assertEqual(self.post("/api/streams", body)[0], 400, body)
        self.assertEqual(self.post("/api/nope", {})[0], 404)


class AddStreamTests(GuiTestCase):
    destinations = True

    def test_valid_stream_is_added_and_the_real_loader_sees_two(self):
        before = self.config.read_text(encoding="utf-8")
        status, payload = self.add("Second stream", "https://example.invalid/live/second")
        self.assertEqual((status, payload), (200, {"ok": True, "id": "second-stream"}))
        config = dead_air.load_config(self.config)
        self.assertEqual([s.id for s in config.streams], ["sample-channel", "second-stream"])
        added = config.streams[1]
        self.assertEqual((added.label, added.url, added.source), ("Second stream", "https://example.invalid/live/second", "direct"))
        self.assertEqual(added.unhealthy_after_s, config.streams[0].unhealthy_after_s)  # the example's defaults
        self.assertEqual([d.id for d in config.destinations], ["ops-ntfy", "ops-discord"])  # destinations untouched
        self.assertTrue(self.config.read_text(encoding="utf-8").startswith(before.rstrip("\n")))
        self.assertEqual(sorted(p.name for p in self.dir.iterdir() if p.name != "state"), ["dead-air.toml"])
        self.assertEqual(self.get_json()["stream_count"], 2)

    def test_quotes_backslashes_and_unicode_in_a_name_survive_toml_quoting(self):
        name = 'Say "hi" \\ Día de Muertos'
        self.assertEqual(self.add(name, "https://example.invalid/live/q")[0], 200)
        self.assertEqual(dead_air.load_config(self.config).streams[1].label, name)

    def test_youtube_source_follows_the_loaders_youtube_rules(self):
        self.assertEqual(self.add("Tube", "https://www.youtube.com/watch?v=abc123", "youtube")[0], 200)
        status, payload = self.add("Not tube", "https://example.invalid/watch?v=1", "youtube")
        self.assertEqual(status, 400)
        self.assertIn("YouTube", payload["error"])

    def test_duplicates_are_409_and_leave_the_file_alone(self):
        before = self.config.read_bytes()
        self.assertEqual(self.add("Another", "https://example.invalid/live/sample")[0], 409)  # same url
        self.assertEqual(self.add("sample CHANNEL", "https://example.invalid/live/other")[0], 409)  # same name
        self.assertEqual(self.add("Sample-channel", "https://example.invalid/live/other")[0], 409)  # same id
        self.assertEqual(self.config.read_bytes(), before)

    def test_invalid_input_is_400_with_the_loaders_message_and_leaves_the_file_alone(self):
        before = self.config.read_bytes()
        cases = [
            ("Bad", "ftp://example.invalid/x", "direct"),
            ("Bad", "https://user:pw@example.invalid/x", "direct"),
            ("Bad", "https://example.invalid/x", "rtmp"),
            ("Bad", "", "direct"),
            ("   ", "https://example.invalid/x", "direct"),
            ("x" * 81, "https://example.invalid/x", "direct"),
            ('Inject"\n[watch]\ninterval_s = 1', "https://example.invalid/x", "direct"),
        ]
        for name, url, source in cases:
            status, payload = self.add(name, url, source)
            self.assertEqual(status, 400, (name, url, source))
            self.assertTrue(payload["error"])
        self.assertEqual(self.post("/api/streams", {"name": "n", "url": 5, "source": "direct"})[0], 400)
        self.assertEqual(self.config.read_bytes(), before)
        self.assertEqual(dead_air.load_config(self.config).streams[0].id, "sample-channel")

    def test_a_broken_config_is_never_overwritten(self):
        self.config.write_text("[watch]\n", encoding="utf-8")
        status, payload = self.add()
        self.assertEqual(status, 400)
        self.assertEqual(self.config.read_text(encoding="utf-8"), "[watch]\n")

    def test_the_64th_stream_is_accepted_and_the_65th_is_refused_by_the_loader(self):
        for index in range(2, dead_air.MAX_STREAMS + 1):
            self.assertEqual(self.add(f"Stream {index}", f"https://example.invalid/live/{index}")[0], 200)
        status, payload = self.add("One too many", "https://example.invalid/live/many")
        self.assertEqual(status, 400)
        self.assertIn("streams", payload["error"])
        self.assertEqual(len(dead_air.load_config(self.config).streams), dead_air.MAX_STREAMS)


class FirstRunTests(GuiTestCase):
    create_config = False

    def test_missing_config_starts_empty_and_adding_creates_it_from_the_example(self):
        state = self.get_json()
        self.assertTrue(state["config_missing"])
        self.assertEqual((state["streams"], state["stream_count"], state["destinations"]), ([], 0, []))
        self.assertEqual(self.post("/api/test-alerts", {}), (200, {"results": []}))
        self.assertEqual(self.add("First one", "https://example.invalid/live/first")[0], 200)
        text = self.config.read_text(encoding="utf-8")
        config = dead_air.load_config(self.config)
        self.assertEqual([s.id for s in config.streams], ["first-one"])
        self.assertEqual(config.destinations, ())
        self.assertIn("# Notifications are disabled", text)  # the example's destination notes came along
        self.assertNotIn("sample-channel", text)
        self.assertFalse(self.get_json()["config_missing"])


class FirstRunToolTests(GuiTestCase):
    create_config = False

    def test_a_created_config_points_yt_dlp_at_the_folders_venv_when_it_has_one(self):
        home = self.dir / "program"
        tool = home / ".venv" / ("Scripts" if os.name == "nt" else "bin") / ("yt-dlp.exe" if os.name == "nt" else "yt-dlp")
        tool.parent.mkdir(parents=True)
        tool.write_bytes(b"")
        shutil.copy(EXAMPLE, home / "dead-air.example.toml")
        with patch.object(dead_air_gui, "HERE", home):
            self.assertEqual(self.add("With venv", "https://example.invalid/live/venv")[0], 200)
        self.assertEqual(dead_air.load_config(self.config).tools.yt_dlp, str(tool))
        self.assertEqual(dead_air.load_config(self.config).tools.ffmpeg, "ffmpeg")

    def test_without_a_venv_the_example_value_is_kept(self):
        self.assertEqual(self.add("No venv", "https://example.invalid/live/novenv")[0], 200)
        self.assertEqual(dead_air.load_config(self.config).tools.yt_dlp, "yt-dlp")


class TestAlertTests(GuiTestCase):
    destinations = True

    def test_each_destination_gets_one_labelled_event_through_the_existing_sender(self):
        replies = [(True, None, 204, None), (False, "HTTP_STATUS", 500, None)]
        with patch.object(dead_air, "_send_destination_bounded", side_effect=replies) as sender:
            status, payload = self.post("/api/test-alerts", {})
        self.assertEqual(status, 200)
        self.assertEqual(payload["results"], [
            {"id": "ops-ntfy", "type": "ntfy", "ok": True, "detail": "sent"},
            {"id": "ops-discord", "type": "discord", "ok": False, "detail": "HTTP_STATUS (500)"},
        ])
        self.assertEqual(sender.call_count, 2)
        self.assertEqual([call.args[0].id for call in sender.call_args_list], ["ops-ntfy", "ops-discord"])
        event = sender.call_args_list[0].args[1]
        self.assertEqual(event["transition"], "test")
        self.assertRegex(event["message"], r"^DEAD AIR test alert from the GUI, \d{4}-\d\d-\d\dT")
        self.assertEqual(dead_air._notification_summary(event),
                         f"Dead Air test: alert from the GUI ({event['event_id']})")
        self.assertEqual(sender.call_args_list[0].kwargs["timeout_s"], dead_air.NOTIFICATION_ATTEMPT_BUDGET_S)
        self.assertNotIn("DEAD_AIR_TEST", json.dumps(payload))

    def test_the_real_sender_fails_closed_when_the_endpoint_variable_is_not_set(self):
        # No patch: the bounded sender runs, finds no endpoint variable, and returns before any network call.
        # A roomier attempt budget keeps a slow cold interpreter start from turning this into a timeout test.
        with patch.dict(os.environ), patch.object(dead_air, "NOTIFICATION_ATTEMPT_BUDGET_S", 30.0):
            os.environ.pop("DEAD_AIR_TEST_NTFY_URL", None)
            os.environ.pop("DEAD_AIR_TEST_DISCORD_URL", None)
            status, payload = self.post("/api/test-alerts", {})
        self.assertEqual(status, 200)
        self.assertEqual([(r["ok"], r["detail"]) for r in payload["results"]], [(False, "ENDPOINT_UNAVAILABLE")] * 2)

    def test_a_page_load_or_state_poll_never_sends(self):
        with patch.object(dead_air, "_send_destination_bounded") as sender:
            self.request("GET", "/")
            self.get_json()
            self.add()
        sender.assert_not_called()


FAKE_WATCHER = "import time\nwhile True:\n    print('{\"observations\": []}', flush=True)\n    time.sleep(0.2)\n"


class WatcherTests(GuiTestCase):
    """The page owns one watcher child; here the child is a tiny script that prints a JSON line every 0.2 s."""

    def setUp(self):
        super().setUp()
        self.script = self.dir / "fake_watch.py"
        self.script.write_text(FAKE_WATCHER, encoding="utf-8")
        patcher = patch.object(dead_air_gui, "watch_argv", lambda _config: [sys.executable, str(self.script)])
        patcher.start()
        self.addCleanup(patcher.stop)

    def watch(self, action):
        return self.post("/api/watch", {"action": action})

    def wait_for(self, condition, seconds=10):
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            if condition():
                return True
            time.sleep(0.1)
        return False

    def test_start_reports_a_running_pid_and_the_last_log_line_then_stop_ends_the_child(self):
        self.assertEqual(self.get_json()["watcher"], {"running": False, "pid": None, "started_at": None, "last_line": None})
        self.assertEqual(self.watch("start"), (200, {"ok": True}))
        child = self.server.child
        info = self.get_json()["watcher"]
        self.assertEqual((info["running"], info["pid"]), (True, child.pid))
        self.assertRegex(info["started_at"], r"Z$")
        self.assertTrue(self.wait_for(lambda: self.get_json()["watcher"]["last_line"] == {"observations": []}))
        self.assertEqual(self.watch("start")[0], 409)
        self.assertEqual(self.watch("stop"), (200, {"ok": True}))
        self.assertTrue(self.wait_for(lambda: child.poll() is not None))
        self.assertEqual(self.get_json()["watcher"]["running"], False)
        self.assertEqual(self.watch("stop")[0], 200)  # stopping nothing is fine

    def test_start_is_refused_while_another_process_holds_the_state(self):
        self.state.parent.mkdir(parents=True)
        (self.state.parent / ".dead-air.lock").write_text("token\tpid=1\n")
        status, payload = self.watch("start")
        self.assertEqual(status, 409)
        self.assertIn("another process", payload["error"])
        self.assertIsNone(self.server.child)

    def test_a_killed_watchers_own_lock_is_removed_on_stop_but_nobody_elses(self):
        self.assertEqual(self.watch("start")[0], 200)
        lock = self.state.parent / ".dead-air.lock"
        lock.write_text(f"token\tpid={self.server.child.pid}\n")
        self.assertEqual(self.watch("stop")[0], 200)
        self.assertFalse(lock.exists())

    def test_watch_needs_a_known_action_a_valid_config_and_the_token(self):
        self.assertEqual(self.watch("restart")[0], 400)
        self.assertEqual(self.post("/api/watch", {"action": "start"}, token=False)[0], 403)
        self.config.write_text("[watch]\n", encoding="utf-8")
        self.assertEqual(self.watch("start")[0], 400)

    def test_quit_stops_the_child_and_the_server(self):
        self.assertEqual(self.watch("start")[0], 200)
        child = self.server.child
        self.assertEqual(self.post("/api/quit", {}), (200, {"ok": True}))
        self.assertTrue(self.wait_for(lambda: child.poll() is not None))
        self.assertTrue(self.wait_for(self._refuses_connections))

    def _refuses_connections(self):
        try:
            self.request("GET", "/api/ping")
        except OSError:
            return True
        return False

    def test_ping_names_the_app_and_still_checks_the_host(self):
        self.assertEqual(self.get_json("/api/ping"), {"app": "dead-air"})
        self.assertEqual(self.request("GET", "/api/ping", headers={"Host": "evil.example"})[0], 421)

    def test_quit_without_the_token_is_403_and_stops_nothing(self):
        self.assertEqual(self.watch("start")[0], 200)
        self.assertEqual(self.post("/api/quit", {}, token=False)[0], 403)
        self.assertIsNone(self.server.child.poll())

    def _idle_server(self, idle_s):
        server = dead_air_gui.make_server(self.config, 0, idle_s)
        serving = threading.Thread(target=server.serve_forever, daemon=True)
        serving.start()
        threading.Thread(target=dead_air_gui._idle_loop, args=(server,), daemon=True).start()
        self.addCleanup(server.server_close)
        self.addCleanup(serving.join, 5)
        self.addCleanup(server.shutdown)
        self.addCleanup(server.done.set)
        self.addCleanup(dead_air_gui.stop_watch, server)
        return server, serving

    def test_idle_timeout_stops_a_server_with_no_watcher(self):
        _server, serving = self._idle_server(1)
        serving.join(10)
        self.assertFalse(serving.is_alive())

    def test_idle_timeout_does_not_fire_while_the_watcher_runs(self):
        server, serving = self._idle_server(1)
        self.assertEqual(dead_air_gui.start_watch(server)[0], 200)
        time.sleep(3)
        self.assertTrue(serving.is_alive())
        dead_air_gui.stop_watch(server)
        serving.join(10)  # with the child gone the idle clock runs out
        self.assertFalse(serving.is_alive())


class InstanceTests(GuiTestCase):
    def test_a_second_instance_opens_the_page_of_the_first_and_exits_0(self):
        out = io.StringIO()
        with patch.object(dead_air_gui.webbrowser, "open") as opener, contextlib.redirect_stdout(out):
            self.assertEqual(dead_air_gui.serve(self.config, self.port, True), 0)
        opener.assert_called_once_with(f"http://127.0.0.1:{self.port}/")
        self.assertIn(f"DEAD AIR GUI: http://127.0.0.1:{self.port}/", out.getvalue())

    def test_a_foreign_listener_on_the_port_exits_2_with_a_message(self):
        import socket
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            listener.listen(1)
            out = io.StringIO()
            with patch.object(dead_air_gui.webbrowser, "open") as opener, contextlib.redirect_stdout(out):
                self.assertEqual(dead_air_gui.serve(self.config, listener.getsockname()[1], True), 2)
        opener.assert_not_called()
        self.assertIn("in use by another program", out.getvalue())

    def test_the_default_port_and_the_watch_flag_reach_serve(self):
        with patch.object(dead_air_gui, "serve", return_value=0) as served:
            self.assertEqual(dead_air.main(["gui", "--config", "x.toml", "--no-browser", "--watch"]), 0)
            self.assertEqual(dead_air.main(["gui", "--config", "x.toml"]), 0)
        self.assertEqual(served.call_args_list[0].args, ("x.toml", 47824, False, True))
        self.assertEqual(served.call_args_list[1].args, ("x.toml", 47824, True, False))

    def test_serve_with_watch_starts_the_watcher_and_stops_it_on_exit(self):
        with patch.object(dead_air_gui.ThreadingHTTPServer, "serve_forever", side_effect=KeyboardInterrupt), \
                patch.object(dead_air_gui, "start_watch", return_value=(200, {"ok": True})) as started, \
                patch.object(dead_air_gui, "stop_watch") as stopped, contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(dead_air_gui.serve(self.config, 0, False, True), 0)
        started.assert_called_once()
        stopped.assert_called_once()


class ServeTests(unittest.TestCase):
    def test_interrupt_exits_zero_prints_the_url_and_opens_the_browser_only_when_asked(self):
        for open_browser in (True, False):
            with patch.object(dead_air_gui.ThreadingHTTPServer, "serve_forever", side_effect=KeyboardInterrupt), \
                    patch.object(dead_air_gui.webbrowser, "open") as opener, \
                    contextlib.redirect_stdout(io.StringIO()) as out:
                self.assertEqual(dead_air_gui.serve("missing.toml", 0, open_browser), 0)
            self.assertRegex(out.getvalue(), r"^DEAD AIR GUI: http://127\.0\.0\.1:\d+/\n$")
            self.assertEqual(opener.called, open_browser)


class CommandTests(unittest.TestCase):
    def test_gui_subcommand_prints_the_url_serves_it_and_stops_on_interrupt(self):
        with tempfile.TemporaryDirectory() as temp:
            config = Path(temp) / "dead-air.toml"
            config.write_text(EXAMPLE.read_text(encoding="utf-8"), encoding="utf-8")
            process = subprocess.Popen(
                [sys.executable, "dead_air.py", "gui", "--config", str(config), "--no-browser", "--port", "0"],
                cwd=ROOT, stdout=subprocess.PIPE, text=True,
                creationflags=getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0))
            try:
                line = process.stdout.readline().strip()
                match = re.fullmatch(r"DEAD AIR GUI: http://127\.0\.0\.1:(\d+)/", line)
                self.assertIsNotNone(match, line)
                connection = http.client.HTTPConnection("127.0.0.1", int(match.group(1)), timeout=10)
                connection.request("GET", "/")
                self.assertEqual(connection.getresponse().status, 200)
                connection.close()
                if os.name != "nt":  # Windows delivers Ctrl+C only to a console, so only POSIX can send it here
                    process.send_signal(signal.SIGINT)
                    self.assertEqual(process.wait(timeout=10), 0)
            finally:
                if process.poll() is None:
                    process.kill()
                process.wait()
                process.stdout.close()

    def test_a_port_that_cannot_be_bound_exits_3_with_a_message(self):
        result = subprocess.run([sys.executable, "dead_air.py", "gui", "--no-browser", "--port", "70000"],
                                cwd=ROOT, capture_output=True, text=True, timeout=30)
        self.assertEqual(result.returncode, 3)
        self.assertIn('"ok": false', result.stdout)

    def test_other_commands_do_not_load_the_gui(self):
        code = "import sys, dead_air; dead_air.main(['validate','--config','dead-air.example.toml']); print('dead_air_gui' in sys.modules)"
        result = subprocess.run([sys.executable, "-c", code], cwd=ROOT, capture_output=True, text=True, timeout=30)
        self.assertEqual(result.stdout.split()[-1], "False")


if __name__ == "__main__":
    unittest.main()
