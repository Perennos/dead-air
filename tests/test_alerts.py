import dataclasses
import json
import tempfile
import tomllib
import unittest
from pathlib import Path
from unittest.mock import patch

import dead_air
from test_dead_air import EXAMPLE, test_config


EVENT = {
    "event_id": "evt-123", "incident_id": "inc-123", "stream_id": "sample-channel",
    "condition": "audio_silence", "transition": "open",
    "observed_at": "2026-10-23T00:00:00Z", "status": "UNHEALTHY",
    "reason_codes": ["AUDIO_SILENCE"],
}
NETWORK = dead_air.load_config(EXAMPLE).network


class FakeSMTP:
    def __init__(self, *args, **kwargs):
        self.args = args
        self.kwargs = kwargs
        self.tls_started = False
        self.message = None
        self.logged_in = False
        self.refused = None
        self.closed = False

    def ehlo(self):
        return 250

    def starttls(self, context=None):
        self.tls_started = context is not None

    def send_message(self, message):
        self.message = message
        return self.refused

    def login(self, *_):
        self.logged_in = True

    def quit(self):
        return 221

    def close(self):
        self.closed = True


class FakeClock:
    def __init__(self, value=0.0):
        self.value = value

    def __call__(self):
        return self.value


class FakePipe:
    def __init__(self):
        self.closed = False

    def close(self):
        self.closed = True


class StalledNotificationProcess:
    def __init__(self, clock):
        self.clock = clock
        self.stdin = FakePipe()
        self.stdout = FakePipe()
        self.stderr = None
        self.returncode = None
        self.terminated = False
        self.input = None

    def communicate(self, input=None, timeout=None):
        self.input = input
        self.clock.value += timeout
        raise dead_air.subprocess.TimeoutExpired("fake-notification-worker", timeout)

    def poll(self):
        return self.returncode

    def terminate(self):
        self.terminated = True
        self.returncode = -15

    def kill(self):
        self.returncode = -9

    def wait(self, timeout=None):
        self.clock.value += 0.05
        return self.returncode


class InterruptedNotificationProcess:
    def __init__(self, clock):
        self.clock = clock
        self.stdin = FakePipe()
        self.stdout = FakePipe()
        self.stderr = None
        self.returncode = None
        self.terminated = False
        self.wait_calls = 0

    def communicate(self, input=None, timeout=None):
        raise KeyboardInterrupt

    def poll(self):
        return self.returncode

    def terminate(self):
        self.terminated = True

    def kill(self):
        self.returncode = -9

    def wait(self, timeout=None):
        self.wait_calls += 1
        self.clock.value += 0.05
        self.returncode = -15
        return self.returncode


class AlertTests(unittest.TestCase):
    def test_destination_config_uses_named_env_and_rejects_partial_smtp_auth(self):
        data = tomllib.loads(EXAMPLE.read_text(encoding="utf-8"))
        data["destinations"] = [{"id": "ops", "type": "discord", "endpoint_env": "DISCORD_WEBHOOK_URL"}]
        config = dead_air.validate_config(data)
        self.assertEqual(config.destinations[0].id, "ops")
        data["destinations"][0]["endpoint_env"] = "url;unsafe"
        with self.assertRaises(dead_air.ConfigError):
            dead_air.validate_config(data)

        data["destinations"] = [{"id": "local-hook", "type": "webhook", "endpoint_env": "LOCAL_HOOK"}]
        self.assertFalse(dead_air.validate_config(data).destinations[0].allow_private)
        data["destinations"][0]["allow_private"] = True
        self.assertTrue(dead_air.validate_config(data).destinations[0].allow_private)
        data["destinations"][0]["allow_private"] = "yes"
        with self.assertRaises(dead_air.ConfigError):
            dead_air.validate_config(data)
        data["destinations"] = [{
            "id": "mail", "type": "smtp", "host": "mail.example.invalid", "port": 587,
            "tls": "starttls", "sender": "dead-air@example.invalid",
            "recipients": ["ops@example.invalid"], "username_env": "SMTP_USER",
        }]
        with self.assertRaises(dead_air.ConfigError):
            dead_air.validate_config(data)

    def test_ntfy_discord_and_webhook_payloads_are_stable_and_redacted(self):
        seen = []

        def http(url, body, headers, timeout):
            seen.append((url, body, headers, timeout))
            return 204, None

        cases = (
            (dead_air.DestinationConfig("n", "ntfy", endpoint_env="NTFY_URL", token_env="NTFY_TOKEN"),
             {"NTFY_URL": "https://ntfy.invalid/topic/private", "NTFY_TOKEN": "secret-token"}),
            (dead_air.DestinationConfig("d", "discord", endpoint_env="DISCORD_URL"),
             {"DISCORD_URL": "https://discord.invalid/api/webhooks/private/token"}),
            (dead_air.DestinationConfig("w", "webhook", endpoint_env="WEBHOOK_URL"),
             {"WEBHOOK_URL": "https://hooks.invalid/secret", "HOOK_TOKEN": "private"}),
        )
        for destination, env in cases:
            if destination.kind == "webhook":
                destination = dataclasses.replace(destination, token_env="HOOK_TOKEN")
            ok, _, status, _ = dead_air.send_destination(
                destination, EVENT, NETWORK, environment=env, http_transport=http,
                address_resolver=lambda _host, _port: ["8.8.8.8"],
            )
            self.assertTrue(ok)
            self.assertEqual(status, 204)
        self.assertEqual([item[3] for item in seen], [5, 5, 5])
        self.assertEqual(seen[0][2]["Authorization"], "Bearer secret-token")
        discord = json.loads(seen[1][1])
        self.assertEqual(discord["allowed_mentions"], {"parse": []})
        self.assertIn("evt-123", discord["content"])
        self.assertEqual(json.loads(seen[2][1])["event_id"], "evt-123")
        self.assertNotIn("https://", "".join(str(x) for x in seen[0][1:]))

    def test_smtp_starttls_is_required_and_message_has_no_header_injection(self):
        client = FakeSMTP()
        smtp = dead_air.DestinationConfig(
            "mail", "smtp", host="mail.example.invalid", port=587, tls="starttls",
            sender="dead-air@example.invalid", recipients=("ops@example.invalid",),
        )
        ok, reason, _, _ = dead_air.send_destination(
            smtp, EVENT, NETWORK, address_resolver=lambda *_: ["8.8.8.8"],
            smtp_factory=lambda *args, **kwargs: client,
        )
        self.assertTrue(ok, reason)
        self.assertTrue(client.tls_started)
        self.assertIn("evt-123", client.message.get_content())
        self.assertFalse(client.logged_in)

    def test_private_destination_requires_opt_in_exact_allowlist_and_tls(self):
        private_network = dead_air.NetworkConfig(("10.0.0.8:465",), 3, 1048576)
        smtp = dead_air.DestinationConfig(
            "mail", "smtp", host="10.0.0.8", port=465, tls="ssl",
            sender="dead-air@example.invalid", recipients=("ops@example.invalid",),
            allow_private=True,
        )
        allowed = dead_air.send_destination(
            smtp, EVENT, private_network, address_resolver=lambda *_: ["10.0.0.8"],
            smtp_ssl_factory=lambda *a, **k: FakeSMTP(),
        )
        self.assertTrue(allowed[0])
        denied = dead_air.send_destination(
            dataclasses.replace(smtp, allow_private=False), EVENT, private_network,
            address_resolver=lambda *_: ["10.0.0.8"], smtp_ssl_factory=lambda *a, **k: FakeSMTP(),
        )
        self.assertEqual(denied, (False, "SMTP_DELIVERY_FAILED", None, None))
        wrong_port = dead_air.NetworkConfig(("10.0.0.8:587",), 3, 1048576)
        denied_port = dead_air.send_destination(
            smtp, EVENT, wrong_port, address_resolver=lambda *_: ["10.0.0.8"],
            smtp_ssl_factory=lambda *a, **k: FakeSMTP(),
        )
        self.assertEqual(denied_port, (False, "SMTP_DELIVERY_FAILED", None, None))

        class Response:
            status = 204
            headers = {}

            def close(self):
                pass

        class Opener:
            def open(self, *_args, **_kwargs):
                return Response()

        webhook = dead_air.DestinationConfig("local-hook", "webhook", endpoint_env="URL", allow_private=True)
        private_https = dead_air.NetworkConfig(("10.0.0.8:443",), 3, 1048576)
        with patch("dead_air.urllib.request.build_opener", return_value=Opener()):
            allowed_http = dead_air.send_destination(
                webhook, EVENT, private_https, environment={"URL": "https://10.0.0.8/notify"},
                address_resolver=lambda *_: ["10.0.0.8"],
            )
            denied_http = dead_air.send_destination(
                dataclasses.replace(webhook, allow_private=False), EVENT, private_https,
                environment={"URL": "https://10.0.0.8/notify"},
                address_resolver=lambda *_: ["10.0.0.8"],
            )
        self.assertEqual(allowed_http, (True, None, 204, None))
        self.assertEqual(denied_http, (False, "PRIVATE_NETWORK_DENIED", None, None))

    def test_smtp_uses_verified_ssl_and_never_falls_back_after_starttls_failure(self):
        calls = []
        client = FakeSMTP()

        def ssl_factory(*args, **kwargs):
            calls.append(("ssl", kwargs.get("context")))
            return client

        smtp_ssl = dead_air.DestinationConfig(
            "mail", "smtp", host="mail.example.invalid", port=465, tls="ssl",
            sender="dead-air@example.invalid", recipients=("ops@example.invalid",),
        )
        ok, _, _, _ = dead_air.send_destination(
            smtp_ssl, EVENT, NETWORK, address_resolver=lambda *_: ["8.8.8.8"],
            smtp_ssl_factory=ssl_factory,
        )
        self.assertTrue(ok)
        self.assertTrue(calls[0][1].check_hostname)
        bad_tls = FakeSMTP()
        bad_tls.starttls = lambda **_: (_ for _ in ()).throw(OSError("private detail"))
        fallback_calls = []
        result = dead_air.send_destination(
            dataclasses.replace(smtp_ssl, tls="starttls"), EVENT, NETWORK,
            address_resolver=lambda *_: ["8.8.8.8"],
            smtp_factory=lambda *a, **k: bad_tls,
            smtp_ssl_factory=lambda *a, **k: fallback_calls.append(True),
        )
        self.assertEqual(result, (False, "SMTP_DELIVERY_FAILED", None, None))
        self.assertEqual(fallback_calls, [])

    def test_email_header_crlf_is_rejected(self):
        with self.assertRaises(dead_air.ConfigError):
            dead_air._email_address("ops@example.invalid\r\nBcc: attacker@example.invalid", "sender")

    def test_http_redirect_is_rejected_and_error_does_not_echo_secret_endpoint(self):
        endpoint = "https://hooks.invalid/webhook/secret-token"
        self.assertEqual(
            dead_air.bounded_http_post("http://hooks.invalid/hook", b"{}", {}, 5, NETWORK),
            (None, "TLS_REQUIRED", None),
        )

        class RedirectOpener:
            def open(self, request, timeout):
                raise dead_air.urllib.error.HTTPError(request.full_url, 302, "redirect", {"Retry-After": "300"}, None)

        with patch("dead_air.urllib.request.build_opener", return_value=RedirectOpener()):
            result = dead_air.bounded_http_post(
                endpoint, b"{}", {"Content-Type": "application/json"}, 5, NETWORK,
                address_resolver=lambda *_: ["8.8.8.8"],
            )
        self.assertEqual(result, (302, "HTTP_STATUS", 120.0))
        self.assertNotIn("secret-token", result[1])

    def test_attempts_are_finite_destinations_are_isolated_and_success_survives_restart(self):
        base = test_config()
        stream = dataclasses.replace(base.streams[0], min_bad_samples=1, unhealthy_after_s=0)
        destinations = (
            dead_air.DestinationConfig("a-fails", "webhook", endpoint_env="A"),
            dead_air.DestinationConfig("b-succeeds", "ntfy", endpoint_env="B"),
        )
        cfg = dataclasses.replace(base, streams=(stream,), destinations=destinations)
        observation = dead_air.Observation(
            stream.id, "2026-10-23T00:00:00Z", "UNHEALTHY", ("AUDIO_SILENCE",),
            dead_air.DimensionEvidence("UNHEALTHY", ("AUDIO_SILENCE",)),
            dead_air.DimensionEvidence("HEALTHY", ()),
        )
        calls = []

        def sender(destination, event, network):
            calls.append(destination.id)
            return (destination.id == "b-succeeds", "HTTP_STATUS", 503 if destination.id == "a-fails" else 204)

        with tempfile.TemporaryDirectory() as td:
            path = Path(td) / "state.db"
            with dead_air.IncidentStore(path, retention_days=1) as store:
                store.record_cycle(cfg, (observation,), 1000, 0)
                dead_air.dispatch_due(store, destinations, NETWORK, now_wall=1000, sender=sender)
                self.assertEqual(set(calls), {"a-fails", "b-succeeds"})
                dead_air.dispatch_due(store, destinations, NETWORK, now_wall=1029, sender=sender)
                self.assertEqual(calls.count("a-fails"), 1)
                dead_air.dispatch_due(store, destinations, NETWORK, now_wall=1030, sender=sender)
                dead_air.dispatch_due(store, destinations, NETWORK, now_wall=1120, sender=sender)
                rows = {row[1]: row[2:] for row in store.deliveries()}
                self.assertEqual(rows["a-fails"][0:2], ("failed", 3))
                self.assertEqual(rows["b-succeeds"][0:2], ("sent", 1))
            with dead_air.IncidentStore(path, retention_days=1) as reopened:
                self.assertTrue(any(row[1] == "b-succeeds" and row[2] == "sent" for row in reopened.deliveries()))
                before = calls.count("b-succeeds")
                dead_air.dispatch_due(reopened, destinations, NETWORK, now_wall=100000, sender=sender)
                self.assertEqual(calls.count("b-succeeds"), before)
            with dead_air.IncidentStore(path, retention_days=1) as store:
                healthy = dead_air.Observation(
                    stream.id, "2026-10-23T00:01:00Z", "HEALTHY", (),
                    dead_air.DimensionEvidence("HEALTHY", ()),
                    dead_air.DimensionEvidence("HEALTHY", ()),
                )
                store.record_cycle(cfg, (healthy,), 1200, 1)
                store.record_cycle(cfg, (healthy,), 1201, 2)
                store.record_cycle(cfg, (healthy,), 90000, 3)  # retention cannot erase failed delivery
                self.assertTrue(any(row[2] == "failed" for row in store.deliveries()))

    def test_removed_destination_does_not_consume_due_quota_and_can_be_reintroduced(self):
        base = test_config()
        streams = tuple(dataclasses.replace(base.streams[0], id=f"channel-{index}",
                                            min_bad_samples=1, unhealthy_after_s=0,
                                            recovery_samples=1) for index in range(8))
        old_destination = dead_air.DestinationConfig("old-target", "webhook", endpoint_env="OLD_URL")
        replacement = dead_air.DestinationConfig("replacement", "webhook", endpoint_env="NEW_URL")
        old_config = dataclasses.replace(base, streams=streams, destinations=(old_destination,))
        new_config = dataclasses.replace(old_config, destinations=(replacement,))
        bad = tuple(dead_air.Observation(
            stream.id, f"2026-10-23T00:00:0{index}Z", "UNHEALTHY", ("AUDIO_SILENCE",),
            dead_air.DimensionEvidence("UNHEALTHY", ("AUDIO_SILENCE",), sampled_s=2, decoded_s=2),
            dead_air.DimensionEvidence("HEALTHY", ("VIDEO_DECODED_NO_EVENT",), sampled_s=2, decoded_s=2),
        ) for index, stream in enumerate(streams))
        with tempfile.TemporaryDirectory() as td, dead_air.IncidentStore(Path(td) / "state.db") as store:
            store.record_cycle(old_config, bad, 100, 0)
            self.assertEqual(len(store.due_deliveries(100)), 8)
            healthy = dead_air.Observation(
                streams[0].id, "2026-10-23T00:00:10Z", "HEALTHY", (),
                dead_air.DimensionEvidence("HEALTHY", ("AUDIO_DECODED_NO_EVENT",), sampled_s=2, decoded_s=2),
                dead_air.DimensionEvidence("HEALTHY", ("VIDEO_DECODED_NO_EVENT",), sampled_s=2, decoded_s=2),
            )
            store.record_cycle(new_config, (healthy,), 101, 1)
            store.record_cycle(new_config, (bad[0],), 102, 2)
            calls = []
            dead_air.dispatch_due(store, (replacement,), NETWORK, now_wall=102,
                                  sender=lambda destination, *_: calls.append(destination.id) or (True, None, 204))
            self.assertEqual(calls, ["replacement", "replacement"])
            old_rows = [row for row in store.deliveries() if row[1] == "old-target"]
            self.assertEqual(len(old_rows), 8)
            self.assertTrue(all(row[2] == "pending" and row[3] == 0 for row in old_rows))
            resumed = []
            dead_air.dispatch_due(store, (old_destination,), NETWORK, now_wall=102,
                                  sender=lambda destination, *_: resumed.append(destination.id) or (True, None, 204))
            self.assertEqual(len(resumed), 8)
            self.assertTrue(all(row[2] == "sent" and row[3] == 1
                                for row in store.deliveries() if row[1] == "old-target"))

    def test_partial_smtp_refusal_is_retryable_redacted_at_least_once(self):
        base = test_config()
        stream = dataclasses.replace(base.streams[0], min_bad_samples=1, unhealthy_after_s=0)
        destination = dead_air.DestinationConfig(
            "mail", "smtp", host="mail.example.invalid", port=465, tls="ssl",
            sender="dead-air@example.invalid",
            recipients=("accepted@example.invalid", "rejected@example.invalid"),
        )
        cfg = dataclasses.replace(base, streams=(stream,), destinations=(destination,))
        bad = dead_air.Observation(
            stream.id, "2026-10-23T00:00:00Z", "UNHEALTHY", ("AUDIO_SILENCE",),
            dead_air.DimensionEvidence("UNHEALTHY", ("AUDIO_SILENCE",), sampled_s=2, decoded_s=2),
            dead_air.DimensionEvidence("HEALTHY", ("VIDEO_DECODED_NO_EVENT",), sampled_s=2, decoded_s=2),
        )
        partial = FakeSMTP()
        partial.refused = {"rejected@example.invalid": (550, b"private server detail")}
        accepted = FakeSMTP()

        def sender(_destination, event, network):
            client = partial if len([c for c in (partial, accepted) if c.message is not None]) == 0 else accepted
            return dead_air.send_destination(
                _destination, event, network, address_resolver=lambda *_: ["8.8.8.8"],
                smtp_ssl_factory=lambda *a, **k: client,
            )

        with tempfile.TemporaryDirectory() as td, dead_air.IncidentStore(Path(td) / "state.db") as store:
            store.record_cycle(cfg, (bad,), 100, 0)
            dead_air.dispatch_due(store, (destination,), NETWORK, now_wall=100, sender=sender)
            first = store.deliveries()[0]
            self.assertEqual(first[2:5], ("pending", 1, "SMTP_PARTIAL_FAILURE"))
            self.assertNotIn("rejected@example.invalid", first[4])
            dead_air.dispatch_due(store, (destination,), NETWORK, now_wall=129, sender=sender)
            self.assertIsNone(accepted.message)
            dead_air.dispatch_due(store, (destination,), NETWORK, now_wall=130, sender=sender)
            final = store.deliveries()[0]
            self.assertEqual(final[2:5], ("sent", 2, None))
            self.assertIn(final[0], accepted.message.get_content())

    def test_attempt_deadline_covers_smtp_stages_and_cleanup(self):
        clock = FakeClock()

        class SlowSMTP(FakeSMTP):
            def ehlo(self):
                clock.value += 0.3
                return 250

            def starttls(self, context=None):
                clock.value += 0.8
                self.tls_started = context is not None

        client = SlowSMTP()
        destination = dead_air.DestinationConfig(
            "mail", "smtp", host="mail.example.invalid", port=587, tls="starttls",
            sender="dead-air@example.invalid", recipients=("ops@example.invalid",),
        )
        result = dead_air.send_destination(
            destination, EVENT, NETWORK, deadline=1.0, monotonic=clock,
            address_resolver=lambda *_: ["8.8.8.8"], smtp_factory=lambda *a, **k: client,
        )
        self.assertEqual(result, (False, "DELIVERY_TIMEOUT", None, None))
        self.assertTrue(client.closed)
        self.assertEqual(clock.value, 1.1)

        clock = FakeClock()
        http_destination = dead_air.DestinationConfig("hook", "webhook", endpoint_env="URL")

        def slow_http(*_):
            clock.value += 1.1
            return 204, None

        result = dead_air.send_destination(
            http_destination, EVENT, NETWORK, environment={"URL": "https://hooks.invalid/private"},
            deadline=1.0, monotonic=clock, http_transport=slow_http,
        )
        self.assertEqual(result, (False, "DELIVERY_TIMEOUT", None, None))

    def test_owned_sender_process_is_cancelled_inside_total_deadline(self):
        clock = FakeClock(1.0)
        destination = dead_air.DestinationConfig("hook", "webhook", endpoint_env="URL", token_env="TOKEN")
        process = StalledNotificationProcess(clock)
        popen_options = {}

        def fake_popen(argv, **kwargs):
            popen_options.update(kwargs)
            popen_options["argv"] = argv
            return process

        result = dead_air._send_destination_bounded(
            destination, EVENT, NETWORK, timeout_s=5.0, monotonic=clock,
            environment={"URL": "https://hooks.invalid/private", "TOKEN": "private-token",
                         "UNRELATED_SECRET": "do-not-copy"},
            popen_factory=fake_popen,
        )
        self.assertEqual(result, (False, "DELIVERY_TIMEOUT", None, None))
        self.assertTrue(process.terminated)
        self.assertIsNotNone(process.returncode)
        self.assertTrue(process.stdin.closed)
        self.assertTrue(process.stdout.closed)
        self.assertLessEqual(clock(), 6.0)
        self.assertEqual(popen_options["env"]["TOKEN"], "private-token")
        self.assertNotIn("UNRELATED_SECRET", popen_options["env"])
        self.assertNotIn(b"private-token", process.input)
        self.assertNotIn(b"https://hooks.invalid/private", process.input)

    def test_owned_sender_child_is_cleaned_before_interruption_propagates(self):
        clock = FakeClock(1.0)
        destination = dead_air.DestinationConfig("hook", "webhook", endpoint_env="URL")
        process = InterruptedNotificationProcess(clock)

        with self.assertRaises(KeyboardInterrupt):
            dead_air._send_destination_bounded(
                destination, EVENT, NETWORK, timeout_s=5.0, monotonic=clock,
                environment={"URL": "https://hooks.invalid/private"},
                popen_factory=lambda *_args, **_kwargs: process,
            )
        self.assertTrue(process.terminated)
        self.assertEqual(process.wait_calls, 1)
        self.assertIsNotNone(process.returncode)
        self.assertTrue(process.stdin.closed)
        self.assertTrue(process.stdout.closed)

    def test_dispatch_stops_after_sender_cleanup_failure(self):
        base = test_config()
        stream = dataclasses.replace(base.streams[0], min_bad_samples=1, unhealthy_after_s=0)
        destinations = (
            dead_air.DestinationConfig("first", "webhook", endpoint_env="FIRST_URL"),
            dead_air.DestinationConfig("second", "webhook", endpoint_env="SECOND_URL"),
        )
        cfg = dataclasses.replace(base, streams=(stream,), destinations=destinations)
        bad = dead_air.Observation(
            stream.id, "2026-10-23T00:00:00Z", "UNHEALTHY", ("AUDIO_SILENCE",),
            dead_air.DimensionEvidence("UNHEALTHY", ("AUDIO_SILENCE",), sampled_s=2, decoded_s=2),
            dead_air.DimensionEvidence("HEALTHY", ("VIDEO_DECODED_NO_EVENT",), sampled_s=2, decoded_s=2),
        )
        with tempfile.TemporaryDirectory() as td, dead_air.IncidentStore(Path(td) / "state.db") as store:
            store.record_cycle(cfg, (bad,), 100, 0)
            with patch("dead_air._send_destination_bounded",
                       return_value=(False, "DELIVERY_CLEANUP_FAILED", None, None)) as send:
                with self.assertRaisesRegex(dead_air.PersistenceError,
                                             "DELIVERY_CLEANUP_FAILED"):
                    dead_air.dispatch_due(store, destinations, NETWORK, now_wall=100)
            self.assertEqual(send.call_count, 1)
            rows = store.deliveries()
            self.assertEqual(sum(row[3] == 1 for row in rows), 1)
            self.assertEqual(sum(row[3] == 0 for row in rows), 1)
            failed = next(row for row in rows if row[4] == "DELIVERY_CLEANUP_FAILED")
            self.assertEqual(failed[2:4], ("pending", 1))

    def test_owned_sender_worker_returns_before_network_when_endpoint_is_unset(self):
        destination = dead_air.DestinationConfig("hook", "webhook", endpoint_env="MISSING_URL")
        result = dead_air._send_destination_bounded(
            destination, EVENT, NETWORK, timeout_s=5.0, environment={},
        )
        self.assertEqual(result, (False, "ENDPOINT_UNAVAILABLE", None, None))

    def test_dispatch_monotonic_budget_stops_before_next_due_row(self):
        class Store:
            def __init__(self):
                self.selected = None
                self.recorded = []

            def due_deliveries(self, now_wall, limit=8, destination_ids=None):
                self.selected = (now_wall, limit, destination_ids)
                return [(f"event-{i}", "active", 0, now_wall, json.dumps(EVENT)) for i in range(3)]

            def record_delivery(self, *args):
                self.recorded.append(args)

        clock = FakeClock()
        store = Store()
        destination = dead_air.DestinationConfig("active", "webhook", endpoint_env="URL")
        calls = []

        def sender(*_):
            calls.append(True)
            clock.value += 4.0
            return False, "HTTP_REQUEST_FAILED", None

        dead_air.dispatch_due(store, (destination,), NETWORK, now_wall=100,
                              sender=sender, monotonic=clock, dispatch_budget_s=5.0)
        self.assertEqual(len(calls), 1)
        self.assertEqual(len(store.recorded), 1)
        self.assertEqual(store.selected, (100, 8, ("active",)))

    def test_retry_after_is_honored_with_a_120_second_cap(self):
        base = test_config()
        stream = dataclasses.replace(base.streams[0], min_bad_samples=1, unhealthy_after_s=0)
        destination = dead_air.DestinationConfig("retry", "webhook", endpoint_env="URL")
        cfg = dataclasses.replace(base, streams=(stream,), destinations=(destination,))
        bad = dead_air.Observation(
            stream.id, "2026-10-23T00:00:00Z", "UNHEALTHY", ("AUDIO_SILENCE",),
            dead_air.DimensionEvidence("UNHEALTHY", ("AUDIO_SILENCE",)),
            dead_air.DimensionEvidence("HEALTHY", ()),
        )
        responses = iter(((False, "HTTP_STATUS", 429, 90), (False, "HTTP_STATUS", 429, 999)))
        with tempfile.TemporaryDirectory() as td, dead_air.IncidentStore(Path(td) / "state.db") as store:
            store.record_cycle(cfg, (bad,), 500, 0)
            dead_air.dispatch_due(store, (destination,), NETWORK, now_wall=500, sender=lambda *_: next(responses))
            self.assertEqual(store.due_deliveries(589), [])
            self.assertEqual(len(store.due_deliveries(590)), 1)
            dead_air.dispatch_due(store, (destination,), NETWORK, now_wall=590, sender=lambda *_: next(responses))
            self.assertEqual(store.due_deliveries(709), [])
            self.assertEqual(len(store.due_deliveries(710)), 1)


if __name__ == "__main__":
    unittest.main()
