"""Finite native loopback acceptance for HLS, incident state, HTTP and senders.

This suite deliberately uses only owned loopback endpoints. It is intended to
run under Root's separately admitted native job, not during source assembly.
"""

import dataclasses
import json
import re
import shutil
import socket
import ssl
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest import mock
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlsplit

import dead_air


_REAL_POPEN = subprocess.Popen
_WORKER_ARGV = [
    sys.executable, "-c",
    "from dead_air import _notification_worker; raise SystemExit(_notification_worker())",
]
_MAX_FIXTURE_BYTES = 4 * 1024 * 1024
_MAX_REQUESTS = 128
_MAX_POST_BYTES = 64 * 1024


def _handler_for(owner):
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.0"

        def log_message(self, *_args):
            return

        def _reply(self, status, body=b"", content_type="text/plain; charset=utf-8", headers=()):
            self.send_response(status)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Connection", "close")
            for name, value in headers:
                self.send_header(name, value)
            self.end_headers()
            if body:
                try:
                    self.wfile.write(body)
                    self.wfile.flush()
                except (BrokenPipeError, ConnectionResetError, OSError):
                    pass
            self.close_connection = True

        def do_GET(self):
            if not owner.note_request():
                self._reply(429)
                return
            path = urlsplit(self.path).path
            if owner.role == "collector":
                self._reply(404)
                return
            if path == "/redirect":
                self._reply(302, headers=(("Location", "/small"),))
                return
            if path == "/loop-a":
                self._reply(302, headers=(("Location", "/loop-b"),))
                return
            if path == "/loop-b":
                self._reply(302, headers=(("Location", "/loop-a"),))
                return
            if path == "/cross":
                self._reply(302, headers=(("Location", owner.cross_target),))
                return
            if path == "/small":
                self._reply(200, b"ok")
                return
            if path == "/large":
                self._reply(200, b"x" * 8192)
                return
            if path == "/slow-body":
                self._slow_body(b"slow-body")
                return
            if path == "/slow.m3u8":
                self._slow_body(b"#EXTM3U\n")
                return
            if path == "/endlist.m3u8":
                self._reply(200, b"#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-ENDLIST\n",
                            "application/vnd.apple.mpegurl")
                return
            if path == "/offline":
                self._reply(410, b"offline")
                return
            if path == "/missing":
                self._reply(404, b"missing")
                return
            if path == "/corrupt.ts":
                self._reply(200, b"not a transport stream", "video/mp2t")
                return
            if path == "/current/index.m3u8":
                with owner.mode_lock:
                    mode = owner.mode
                payload = owner.playlists.get(mode)
                if payload is not None:
                    self._reply(200, payload, "application/vnd.apple.mpegurl")
                    return
            payload = owner.files.get(path)
            if payload is not None:
                body, content_type = payload
                self._reply(200, body, content_type)
                return
            self._reply(404)

        def _slow_body(self, prefix):
            body = prefix + b"-late"
            self.send_response(200)
            self.send_header("Content-Type", "application/octet-stream")
            self.send_header("Content-Length", str(len(body) + 16))
            self.send_header("Connection", "close")
            self.end_headers()
            try:
                self.wfile.write(prefix)
                self.wfile.flush()
                owner.stop_event.wait(2.0)
                self.wfile.write(body[len(prefix):])
                self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError, OSError):
                pass
            self.close_connection = True

        def do_POST(self):
            if not owner.note_request():
                self._reply(429)
                return
            if owner.role != "collector" or urlsplit(self.path).path != "/alert":
                self._reply(404)
                return
            try:
                length = int(self.headers.get("Content-Length", "-1"))
            except ValueError:
                self._reply(400)
                return
            if length < 0 or length > _MAX_POST_BYTES:
                self._reply(413)
                return
            try:
                raw = self.rfile.read(length)
                if len(raw) != length:
                    self._reply(400)
                    return
                payload = json.loads(raw.decode("utf-8"))
                event_id = payload.get("event_id")
                if not isinstance(event_id, str) or not re.fullmatch(r"[a-f0-9]{32}", event_id):
                    self._reply(400)
                    return
            except (UnicodeDecodeError, json.JSONDecodeError, AttributeError, OSError):
                self._reply(400)
                return
            with owner.post_lock:
                owner.received_event_ids.append(event_id)
                slow = owner.collector_slow
                status = 503 if not slow and not owner.received_statuses else 204
                if not slow:
                    owner.received_statuses.append(status)
            if slow:
                owner.stop_event.wait(6.0)
                self._reply(204)
            else:
                self._reply(status)

    return Handler


class _OwnedHTTPServer(ThreadingHTTPServer):
    allow_reuse_address = True
    daemon_threads = False
    block_on_close = False

    def __init__(self, role, stop_event, *, files=None, playlists=None):
        self.role = role
        self.stop_event = stop_event
        self.files = files or {}
        self.playlists = playlists or {}
        self.mode = "normal"
        self.mode_lock = threading.Lock()
        self.post_lock = threading.Lock()
        self.collector_slow = False
        self.received_event_ids = []
        self.received_statuses = []
        self.cross_target = ""
        self.request_lock = threading.Lock()
        self.request_count = 0
        self.accepted_lock = threading.Lock()
        self.accepted_sockets = []
        self.handler_lock = threading.Lock()
        self.handler_threads = []
        self.serve_thread = None
        self.started = False
        super().__init__(("127.0.0.1", 0), _handler_for(self), bind_and_activate=True)
        self.timeout = 0.1

    def note_request(self):
        with self.request_lock:
            self.request_count += 1
            return self.request_count <= _MAX_REQUESTS

    def get_request(self):
        request, address = super().get_request()
        request.settimeout(0.5)
        with self.accepted_lock:
            self.accepted_sockets.append(request)
        return request, address

    def process_request_thread(self, request, client_address):
        with self.handler_lock:
            self.handler_threads.append(threading.current_thread())
        return super().process_request_thread(request, client_address)

    def handle_error(self, *_args):
        return

    def start(self):
        self.serve_thread = threading.Thread(
            target=self.serve_forever, kwargs={"poll_interval": 0.05},
            name=f"dead-air-{self.role}-server", daemon=False,
        )
        self.serve_thread.start()
        self.started = True

    @property
    def port(self):
        return int(self.server_address[1])

    def set_mode(self, mode):
        with self.mode_lock:
            self.mode = mode

    def close_owned(self):
        self.stop_event.set()
        with self.accepted_lock:
            accepted = list(self.accepted_sockets)
        for connection in accepted:
            try:
                connection.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            try:
                connection.close()
            except OSError:
                pass
        if self.started:
            self.shutdown()
        self.server_close()
        if self.serve_thread is not None:
            self.serve_thread.join(timeout=2.0)
            if self.serve_thread.is_alive():
                raise AssertionError("owned HTTP server thread did not stop")
        with self.handler_lock:
            handlers = list(self.handler_threads)
        for handler in handlers:
            handler.join(timeout=2.0)
        if any(handler.is_alive() for handler in handlers):
            raise AssertionError("owned HTTP handler thread did not stop")


class _OwnedSMTPListener:
    """A bounded listener that accepts TCP and deliberately sends no greeting."""

    def __init__(self, stop_event):
        self.stop_event = stop_event
        self.listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.listener.bind(("127.0.0.1", 0))
        self.listener.listen(4)
        self.listener.settimeout(0.1)
        self.accepted = []
        self.lock = threading.Lock()
        self.thread = threading.Thread(target=self._serve, name="dead-air-smtp-listener", daemon=False)
        self.started = False

    @property
    def port(self):
        return int(self.listener.getsockname()[1])

    def start(self):
        self.thread.start()
        self.started = True

    def _serve(self):
        while not self.stop_event.is_set():
            try:
                connection, _address = self.listener.accept()
            except socket.timeout:
                continue
            except OSError:
                return
            connection.settimeout(0.2)
            with self.lock:
                self.accepted.append(connection)

    def close_owned(self):
        self.stop_event.set()
        with self.lock:
            accepted = list(self.accepted)
        for connection in accepted:
            try:
                connection.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            try:
                connection.close()
            except OSError:
                pass
        try:
            self.listener.close()
        except OSError:
            pass
        if self.started:
            self.thread.join(timeout=2.0)
            if self.thread.is_alive():
                raise AssertionError("owned SMTP listener thread did not stop")


class _RecordingPopen:
    """Launch the exact supplied child while recording it for cleanup checks."""

    def __init__(self, test_ca=None):
        self.test_ca = str(test_ca) if test_ca is not None else None
        self.launches = []

    def __call__(self, argv, **kwargs):
        forwarded = dict(kwargs)
        environment = dict(forwarded["env"])
        if self.test_ca is not None:
            environment["SSL_CERT_FILE"] = self.test_ca
        forwarded["env"] = environment
        child = _REAL_POPEN(argv, **forwarded)
        self.launches.append({"child": child, "argv": list(argv), "environment": environment})
        return child

    def assert_reaped(self, testcase):
        for launch in self.launches:
            child = launch["child"]
            testcase.assertIsNotNone(child.returncode, "sender child was not reaped")
            testcase.assertEqual(launch["argv"], _WORKER_ARGV)
            for pipe in (child.stdin, child.stdout, child.stderr):
                if pipe is not None:
                    testcase.assertTrue(pipe.closed, "sender child pipe remained open")

    def cleanup_owned(self):
        for launch in self.launches:
            child = launch["child"]
            if child.poll() is None:
                try:
                    child.terminate()
                except OSError:
                    pass
                try:
                    child.wait(timeout=0.2)
                except subprocess.TimeoutExpired:
                    try:
                        child.kill()
                    except OSError:
                        pass
                    try:
                        child.wait(timeout=2.0)
                    except subprocess.TimeoutExpired:
                        raise AssertionError("owned sender child did not stop during fixture cleanup")
            for pipe in (child.stdin, child.stdout, child.stderr):
                if pipe is not None:
                    try:
                        pipe.close()
                    except OSError:
                        pass


class NativeNetworkIntegrationTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        missing = [name for name in ("ffmpeg", "ffprobe", "openssl") if shutil.which(name) is None]
        if missing:
            raise unittest.SkipTest("native integration dependencies unavailable: " + ", ".join(missing))
        cls.ffmpeg = shutil.which("ffmpeg")
        cls.ffprobe = shutil.which("ffprobe")
        cls.openssl = shutil.which("openssl")
        cls.temp = tempfile.TemporaryDirectory(prefix="dead-air-network-acceptance-")
        cls.addClassCleanup(cls.temp.cleanup)
        cls.root = Path(cls.temp.name)
        cls.fixtures = {
            "normal": cls._generate_hls(
                "normal", "testsrc2=size=160x90:rate=10",
                "sine=frequency=1000:sample_rate=48000",
            ),
            "bad": cls._generate_hls(
                "bad", "color=c=black:size=160x90:rate=10",
                "anullsrc=r=48000:cl=stereo",
            ),
        }
        cls._check_fixture_set()
        cls.cert_path, cls.key_path = cls._generate_test_certificate()
        base_path = Path(__file__).parents[1] / "dead-air.example.toml"
        cls.base_config = dead_air.load_config(base_path)

    @classmethod
    def _generate_hls(cls, name, video_source, audio_source):
        directory = cls.root / name
        directory.mkdir()
        playlist = directory / "index.m3u8"
        segment_pattern = directory / "segment-%02d.ts"
        argv = [
            cls.ffmpeg, "-nostdin", "-hide_banner", "-loglevel", "error", "-y",
            "-threads", "1", "-filter_threads", "1", "-filter_complex_threads", "1",
            "-f", "lavfi", "-i", video_source,
            "-f", "lavfi", "-i", audio_source,
            "-t", "12", "-map", "0:v:0", "-map", "1:a:0",
            "-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p", "-threads", "1",
            "-g", "20", "-keyint_min", "20", "-sc_threshold", "0",
            "-c:a", "aac", "-ar", "48000", "-ac", "2", "-b:a", "64k",
            "-f", "hls", "-hls_time", "2", "-hls_list_size", "0", "-hls_flags", "omit_endlist",
            "-hls_segment_filename", str(segment_pattern), str(playlist),
        ]
        result = dead_air.run_bounded(
            argv, 15, max_output_bytes=8192,
            file_limits=((playlist, _MAX_FIXTURE_BYTES),),
        )
        if (result.returncode != 0 or result.timed_out or result.output_limited
                or result.file_limited or not result.cleanup_complete):
            raise RuntimeError(
                f"required native HLS fixture generation failed for {name}; "
                "verify FFmpeg HLS, libx264 and AAC support"
            )
        allowed = {playlist}
        allowed.update(directory.glob("segment-*.ts"))
        actual = set(directory.iterdir())
        if (actual != allowed or not playlist.is_file()
                or not 0 < len(allowed) - 1 <= 6
                or any(not re.fullmatch(r"segment-\d{2}\.ts", path.name) for path in allowed - {playlist})):
            raise RuntimeError(f"native HLS fixture file set invalid for {name}")
        raw = playlist.read_bytes()
        if b"#EXT-X-ENDLIST" in raw:
            raise RuntimeError(f"native live-style HLS fixture unexpectedly ended for {name}")
        lines = raw.decode("utf-8").splitlines()
        routed = []
        for line in lines:
            if line and not line.startswith("#"):
                line = f"/{name}/{Path(line).name}"
            routed.append(line)
        payload = "\n".join(routed).encode("utf-8") + b"\n"
        files = {f"/{name}/{path.name}": (path.read_bytes(), "video/mp2t")
                 for path in allowed if path != playlist}
        files.update({f"/{name}/index.m3u8": (payload, "application/vnd.apple.mpegurl")})
        return {"directory": directory, "files": files, "size": sum(p.stat().st_size for p in allowed)}

    @classmethod
    def _check_fixture_set(cls):
        total = sum(item["size"] for item in cls.fixtures.values())
        if total > _MAX_FIXTURE_BYTES:
            raise RuntimeError("combined native HLS fixtures exceed 4 MiB")
        cls.fixture_files = {}
        for item in cls.fixtures.values():
            cls.fixture_files.update(item["files"])

    @classmethod
    def _generate_test_certificate(cls):
        cert_path = cls.root / "loopback-test-cert.pem"
        key_path = cls.root / "loopback-test-key.pem"
        argv = [
            cls.openssl, "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
            "-subj", "/CN=dead-air-loopback-test",
            "-addext", "subjectAltName=IP:127.0.0.1",
            "-addext", "basicConstraints=critical,CA:TRUE",
            "-addext", "keyUsage=critical,digitalSignature,keyEncipherment,keyCertSign,cRLSign",
            "-keyout", str(key_path), "-out", str(cert_path),
        ]
        result = dead_air.run_bounded(
            argv, 15, max_output_bytes=8192,
            file_limits=((key_path, 32768), (cert_path, 32768)),
        )
        if (result.returncode != 0 or result.timed_out or result.output_limited
                or result.file_limited or not result.cleanup_complete
                or not cert_path.is_file() or not key_path.is_file()):
            raise RuntimeError("required bounded OpenSSL loopback certificate preparation failed")
        return cert_path, key_path

    def setUp(self):
        self.stop_event = threading.Event()

    def _own_http(self, role, *, files=None, playlists=None, tls_context=None):
        server = _OwnedHTTPServer(role, self.stop_event, files=files, playlists=playlists)
        self.addCleanup(server.close_owned)
        if tls_context is not None:
            server.socket = tls_context.wrap_socket(
                server.socket, server_side=True, do_handshake_on_connect=False,
            )
        server.start()
        return server

    def _test_tls_context(self):
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.load_cert_chain(str(self.cert_path), str(self.key_path))
        return context

    def _base_network(self, *ports):
        unique = tuple(dict.fromkeys(f"127.0.0.1:{int(port)}" for port in ports))
        return dataclasses.replace(self.base_config.network, private_allowlist=unique)

    def _direct_stream(self, url, network, **overrides):
        watch = dataclasses.replace(
            self.base_config.watch,
            sample_s=3, resolver_timeout_s=0.25, capture_timeout_s=1.5,
            analysis_timeout_s=1.5, cycle_budget_s=5,
            max_sample_bytes=16_000_000, max_output_bytes=262_144,
            state_path=str(self.root / "unused-state.sqlite"),
        )
        stream = dataclasses.replace(
            self.base_config.streams[0], url=url, source="direct", expected_live=True,
            expect_audio=True, expect_video=True, thumbnail_enabled=False,
            offline_http_statuses=tuple(sorted(set(self.base_config.streams[0].offline_http_statuses) | {410})),
            allow_private=True, **overrides,
        )
        tools = dataclasses.replace(self.base_config.tools, ffmpeg=self.ffmpeg, ffprobe=self.ffprobe)
        config = dataclasses.replace(self.base_config, watch=watch, tools=tools,
                                      network=network, streams=(stream,), destinations=())
        return config, stream

    def _own_recorder(self, test_ca=None):
        recorder = _RecordingPopen(test_ca)
        self.addCleanup(recorder.cleanup_owned)
        return recorder

    def test_a_real_hls_incident_https_retry_and_recovery(self):
        playlists = {
            mode: self.fixtures[mode]["files"][f"/{mode}/index.m3u8"][0]
            for mode in ("normal", "bad")
        }
        media = self._own_http("media", files=self.fixture_files, playlists=playlists)
        collector = self._own_http("collector", tls_context=self._test_tls_context())
        network = self._base_network(media.port, collector.port)
        endpoint_name = "DEAD_AIR_TEST_ENDPOINT"
        endpoint = f"https://127.0.0.1:{collector.port}/alert"
        destination = dead_air.DestinationConfig(
            id="loopback-alert", kind="webhook", endpoint_env=endpoint_name, allow_private=True,
        )
        endpoint_environment = {endpoint_name: endpoint}
        base = self.base_config
        watch = dataclasses.replace(
            base.watch, sample_s=3, resolver_timeout_s=1, capture_timeout_s=10,
            analysis_timeout_s=10, cycle_budget_s=25,
            max_sample_bytes=16_000_000, max_output_bytes=262_144,
            state_path=str(self.root / "incidents.sqlite"),
        )
        tools = dataclasses.replace(base.tools, ffmpeg=self.ffmpeg, ffprobe=self.ffprobe)
        stream = dataclasses.replace(
            base.streams[0], url=f"http://127.0.0.1:{media.port}/current/index.m3u8",
            source="direct", expected_live=True, expect_audio=True, expect_video=True,
            detect_freeze=True, freeze_noise=0.001, freeze_window_s=1,
            silence_db=-50, silence_window_s=1, min_bad_samples=2,
            unhealthy_after_s=1, recovery_samples=2, max_gap_s=60,
            unknown_notice_after_s=600, thumbnail_enabled=False, allow_private=True,
        )
        config = dataclasses.replace(base, watch=watch, tools=tools, network=network,
                                     streams=(stream,), destinations=(destination,))

        no_trust = self._own_recorder()
        before_negative = len(collector.received_event_ids)
        negative = dead_air._send_destination_bounded(
            destination, {"event_id": "0" * 32, "transition": "open",
                         "stream_id": stream.id, "condition": "audio_silence"},
            network, timeout_s=3, environment=endpoint_environment, popen_factory=no_trust,
        )
        self.assertFalse(negative[0], "self-signed loopback TLS unexpectedly succeeded without test trust")
        self.assertNotIn("SSL_CERT_FILE", no_trust.launches[0]["environment"])
        self.assertEqual(len(collector.received_event_ids), before_negative,
                         "untrusted loopback TLS reached the HTTP collector")
        no_trust.assert_reaped(self)

        trusted = self._own_recorder(self.cert_path)
        real_send = dead_air._send_destination_bounded

        def real_recorded_send(*args, **kwargs):
            kwargs["popen_factory"] = trusted
            return real_send(*args, **kwargs)

        timestamps = ((100, 10), (101, 11), (131, 41), (132, 42), (133, 43))
        modes = ("bad", "bad", "bad", "normal", "normal")
        observations = []
        state_path = Path(watch.state_path)
        with mock.patch.object(dead_air, "_send_destination_bounded", real_recorded_send):
            with dead_air.IncidentStore(state_path) as store:
                for mode, (now_wall, now_mono) in zip(modes, timestamps):
                    media.set_mode(mode)
                    batch = dead_air.check_cycle(config)
                    self.assertEqual(len(batch), 1)
                    cycle_index = len(observations) + 1
                    diagnostic = json.dumps(
                        {
                            "mode": mode,
                            "cycle_index": cycle_index,
                            "configured_windows_s": {
                                "sample": config.watch.sample_s,
                                "audio_detector": stream.silence_window_s,
                                "video_detector": stream.freeze_window_s,
                            },
                            "observation": dead_air.observation_json(batch[0]),
                        },
                        separators=(",", ":"), sort_keys=True,
                    )[:8192]
                    if mode == "bad":
                        self.assertEqual(batch[0].status, "UNHEALTHY", diagnostic)
                        self.assertEqual(batch[0].video.status, "UNHEALTHY", diagnostic)
                        self.assertIn("VIDEO_FREEZE", batch[0].video.reason_codes, diagnostic)
                        self.assertEqual(batch[0].audio.status, "UNHEALTHY", diagnostic)
                        self.assertIn("AUDIO_SILENCE", batch[0].audio.reason_codes, diagnostic)
                    else:
                        self.assertEqual(batch[0].status, "HEALTHY", diagnostic)
                        self.assertEqual((batch[0].video.status, batch[0].audio.status),
                                         ("HEALTHY", "HEALTHY"), diagnostic)
                    observations.append(batch[0])
                    store.record_cycle(config, batch, now_wall, now_mono)
                    dead_air.dispatch_due(
                        store, config.destinations, config.network, now_wall=now_wall,
                        environment=endpoint_environment,
                    )
                    if len(observations) == 1:
                        self.assertEqual(store.open_incidents(), [])
                    elif len(observations) == 2:
                        self.assertEqual({row[1] for row in store.open_incidents()},
                                         {"audio_silence", "video_freeze"})
                        failed = [row for row in store.deliveries() if row[2] == "pending"]
                        self.assertEqual(len(failed), 1)
                        self.assertIsNotNone(failed[0][4])
                    elif len(observations) == 3:
                        self.assertEqual(len(store.events()), 2)
                    elif len(observations) == 4:
                        self.assertEqual(len(store.open_incidents()), 2)
                    else:
                        self.assertEqual(store.open_incidents(), [])

                event_rows = store.events()
                self.assertEqual(len(event_rows), 4)
                self.assertEqual(len({row[0] for row in event_rows}), 4)
                expected_conditions = {"audio_silence", "video_freeze"}
                self.assertEqual({(row[1], row[2]) for row in event_rows},
                                 {(condition, transition) for condition in expected_conditions
                                  for transition in ("open", "recovery")})
                event_by_key = {(row[1], row[2]): json.loads(row[3]) for row in event_rows}
                for condition, dimension, source in (
                    ("audio_silence", "audio", observations[1].audio),
                    ("video_freeze", "video", observations[1].video),
                ):
                    opened = event_by_key[(condition, "open")]["evidence"]
                    self.assertEqual(opened["threshold"]["sample_count"], 2)
                    self.assertEqual(opened["threshold"]["required_samples"], stream.min_bad_samples)
                    self.assertAlmostEqual(opened["threshold"]["span_s"], 1.0)
                    metrics = opened["affected_dimensions"][dimension]
                    self.assertIsNotNone(source.coverage)
                    self.assertEqual(metrics["status"], source.status)
                    self.assertEqual(metrics["reason_codes"], list(source.reason_codes))
                    self.assertEqual(metrics["sampled_s"], source.sampled_s)
                    self.assertEqual(metrics["decoded_s"], round(source.decoded_s, 3))
                    self.assertEqual(metrics["coverage"]["frame_count" if dimension == "video" else "sample_count"],
                                     source.coverage.frame_count if dimension == "video"
                                     else source.coverage.sample_count)
                    recovered = event_by_key[(condition, "recovery")]["evidence"]
                    self.assertEqual(recovered["threshold"]["sample_count"], 2)
                    self.assertEqual(recovered["threshold"]["required_samples"], stream.recovery_samples)
                    self.assertEqual(len(recovered["samples"]), 2)
                    for sample, actual in zip(recovered["samples"], observations[3:]):
                        sampled_dimension = sample["dimensions"][dimension]
                        expected_dimension = actual.audio if dimension == "audio" else actual.video
                        self.assertEqual(sampled_dimension["status"], expected_dimension.status)
                        self.assertEqual(sampled_dimension["sampled_s"], expected_dimension.sampled_s)
                        self.assertEqual(sampled_dimension["decoded_s"], round(expected_dimension.decoded_s, 3))

                deliveries = store.deliveries()
                self.assertEqual(len(deliveries), 4)
                self.assertTrue(all(row[2] == "sent" and row[5] == 204 for row in deliveries))
                posts = list(zip(collector.received_event_ids, collector.received_statuses))
                self.assertEqual(len(posts), 5)
                self.assertEqual(posts[0][1], 503)
                self.assertEqual([status for _event, status in posts[1:]], [204, 204, 204, 204])
                self.assertEqual(posts[0][0], posts[2][0], "retry changed the stable event ID")
                self.assertEqual(len(trusted.launches), 5)
                self.assertTrue(all("SSL_CERT_FILE" in child["environment"] for child in trusted.launches))
                trusted.assert_reaped(self)

        self.assertEqual(state_path.with_name(".dead-air.lock").read_bytes(), dead_air.IncidentStore.LEGACY_GUARD)
        with dead_air.IncidentStore(state_path) as reopened:
            self.assertEqual(len(reopened.events()), 4)
            self.assertEqual(len(reopened.deliveries()), 4)
            self.assertEqual(reopened.open_incidents(), [])
            self.assertTrue(all(row[2] == "sent" for row in reopened.deliveries()))

    def test_b_http_limits_redirects_off_air_and_unknown(self):
        media = self._own_http("media", files=self.fixture_files)
        second = self._own_http("second", files={"/small": (b"other", "text/plain")})
        media.cross_target = f"http://127.0.0.1:{second.port}/small"
        network = self._base_network(media.port)
        root = f"http://127.0.0.1:{media.port}"

        redirected = dead_air.bounded_http_get(
            root + "/redirect", source="direct", network=network, allow_private=True,
            timeout_s=1, max_bytes=256,
        )
        self.assertEqual((redirected.status, redirected.body), (200, b"ok"))

        looped = dead_air.bounded_http_get(
            root + "/loop-a", source="direct", network=network, allow_private=True,
            timeout_s=1, max_bytes=256,
        )
        self.assertEqual(looped.error_code, "REDIRECT_LIMIT")

        before_second = second.request_count
        cross = dead_air.bounded_http_get(
            root + "/cross", source="direct", network=network, allow_private=True,
            timeout_s=1, max_bytes=256,
        )
        self.assertIsNone(cross.status)
        self.assertEqual(second.request_count, before_second,
                         "cross-port redirect reached the unallowlisted owned server")

        oversized = dead_air.bounded_http_get(
            root + "/large", source="direct", network=network, allow_private=True,
            timeout_s=1, max_bytes=256,
        )
        self.assertTrue(oversized.truncated)
        self.assertLessEqual(len(oversized.body), 256)

        before_denied = media.request_count
        denied = dead_air.bounded_http_get(
            root + "/small", source="direct", network=network, allow_private=False,
            timeout_s=1, max_bytes=256,
        )
        self.assertIsNone(denied.status)
        self.assertEqual(media.request_count, before_denied,
                         "default private-network denial made a loopback request")

        started = time.monotonic()
        slow_http = dead_air.bounded_http_get(
            root + "/slow-body", source="direct", network=network, allow_private=True,
            timeout_s=0.25, max_bytes=256,
        )
        elapsed = time.monotonic() - started
        self.assertLessEqual(elapsed, 1.25, "bounded slow-body request exceeded its scheduling margin")
        self.assertTrue(slow_http.error_code == "HTTP_TIMEOUT" or slow_http.truncated
                        or len(slow_http.body) < len(b"slow-body-late"))

        cases = (
            ("endlist.m3u8", "OFF_AIR", "DIRECT_HLS_ENDLIST"),
            ("offline", "OFF_AIR", "DIRECT_OFFLINE_STATUS"),
            ("missing", "UNKNOWN", None),
            ("slow.m3u8", "UNKNOWN", None),
            ("corrupt.ts", "UNKNOWN", None),
        )
        for route, expected_status, expected_reason in cases:
            with self.subTest(route=route):
                config, stream = self._direct_stream(f"{root}/{route}", network)
                started = time.monotonic()
                result = dead_air.observe_stream(config, stream, deadline=started + 5.0)
                self.assertLessEqual(time.monotonic() - started, 6.0,
                                     "bounded direct observation exceeded its scheduling margin")
                self.assertEqual(result.status, expected_status)
                if expected_reason is not None:
                    self.assertIn(expected_reason, result.reason_codes)
                else:
                    self.assertNotIn("DIRECT_HLS_ENDLIST", result.reason_codes)
                    self.assertNotIn("DIRECT_OFFLINE_STATUS", result.reason_codes)

        self.assertLessEqual(media.request_count, _MAX_REQUESTS)
        self.assertLessEqual(second.request_count, _MAX_REQUESTS)

    def test_https_media_capture_rejects_an_untrusted_certificate(self):
        media = self._own_http("media", files=self.fixture_files, tls_context=self._test_tls_context())
        network = self._base_network(media.port)
        url = f"https://127.0.0.1:{media.port}/normal/index.m3u8"
        config, stream = self._direct_stream(url, network)
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "capture.mkv"
            argv = dead_air.build_capture_argv(
                dead_air.ResolvedMedia((dead_air.MediaInput("muxed", url),)), stream, config, output,
            )
            result = dead_air.run_bounded(argv, 3, max_output_bytes=8192)
        self.assertNotEqual(result.returncode, 0, "FFmpeg accepted an untrusted TLS certificate")
        self.assertIn("certificate", result.stderr.lower())
        self.assertTrue(result.cleanup_complete)

    def test_c_https_and_smtp_children_stop_at_deadline(self):
        collector = self._own_http("collector", tls_context=self._test_tls_context())
        collector.collector_slow = True
        smtp = _OwnedSMTPListener(self.stop_event)
        self.addCleanup(smtp.close_owned)
        smtp.start()
        network = self._base_network(collector.port, smtp.port)
        endpoint_name = "DEAD_AIR_SLOW_ENDPOINT"
        endpoint = f"https://127.0.0.1:{collector.port}/alert"
        event = {"event_id": "1" * 32, "transition": "open",
                 "stream_id": "acceptance", "condition": "audio_silence"}
        recorder = self._own_recorder(self.cert_path)

        webhook = dead_air.DestinationConfig(
            id="slow-loopback-webhook", kind="webhook", endpoint_env=endpoint_name,
            allow_private=True,
        )
        started = time.monotonic()
        response = dead_air._send_destination_bounded(
            webhook, event, network, timeout_s=3,
            environment={endpoint_name: endpoint}, popen_factory=recorder,
        )
        elapsed = time.monotonic() - started
        self.assertFalse(response[0], "stalled HTTPS collector unexpectedly succeeded")
        self.assertLessEqual(elapsed, 4.0, "HTTPS sender exceeded its explicit scheduling margin")
        self.assertEqual(collector.received_event_ids, [event["event_id"]])
        recorder.assert_reaped(self)

        smtp_destination = dead_air.DestinationConfig(
            id="greeting-hang-smtp", kind="smtp", host="127.0.0.1", port=smtp.port,
            tls="starttls", sender="dead-air@example.test",
            recipients=("ops@example.test",), allow_private=True,
        )
        started = time.monotonic()
        smtp_response = dead_air._send_destination_bounded(
            smtp_destination, event, network, timeout_s=3,
            environment={}, popen_factory=recorder,
        )
        elapsed = time.monotonic() - started
        self.assertFalse(smtp_response[0], "SMTP greeting-hang unexpectedly succeeded")
        self.assertLessEqual(elapsed, 4.0, "SMTP sender exceeded its explicit scheduling margin")
        self.assertEqual(len(smtp.accepted), 1, "SMTP child did not reach the owned loopback listener")
        self.assertEqual(len(recorder.launches), 2)
        recorder.assert_reaped(self)


if __name__ == "__main__":
    unittest.main()
