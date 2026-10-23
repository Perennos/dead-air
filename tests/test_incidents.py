import dataclasses
import json
import tempfile
import unittest
from pathlib import Path

import dead_air
from test_dead_air import test_config


def config(*, min_bad=2, unhealthy=10, recovery=2, max_gap=180, unknown_after=600, destinations=()):
    base = test_config()
    stream = dataclasses.replace(
        base.streams[0], min_bad_samples=min_bad, unhealthy_after_s=unhealthy,
        recovery_samples=recovery, max_gap_s=max_gap, unknown_notice_after_s=unknown_after,
    )
    return dataclasses.replace(base, streams=(stream,), destinations=destinations)


def observation(status="UNHEALTHY", *, audio=None, video=None, reasons=(), at="2026-10-23T00:00:00Z"):
    audio = audio or dead_air.DimensionEvidence("HEALTHY", ("AUDIO_DECODED_NO_EVENT",), sampled_s=2, decoded_s=2)
    video = video or dead_air.DimensionEvidence("HEALTHY", ("VIDEO_DECODED_NO_EVENT",), sampled_s=2, decoded_s=2)
    return dead_air.Observation("sample-channel", at, status, tuple(reasons), audio, video)


def silence():
    return dead_air.DimensionEvidence("UNHEALTHY", ("AUDIO_SILENCE",),
                                     (dead_air.Interval(0.25, 1.5),), sampled_s=2, decoded_s=2)


def freeze():
    return dead_air.DimensionEvidence("UNHEALTHY", ("VIDEO_FREEZE",),
                                     (dead_air.Interval(0.25, 1.5),), sampled_s=2, decoded_s=2)


class IncidentTests(unittest.TestCase):
    def test_open_requires_sample_count_and_elapsed_span(self):
        cfg = config(min_bad=2, unhealthy=10)
        with tempfile.TemporaryDirectory() as td, dead_air.IncidentStore(Path(td) / "state.db") as store:
            bad = observation(audio=silence(), reasons=("AUDIO_SILENCE",))
            store.record_cycle(cfg, (bad,), 100, 0)
            store.record_cycle(cfg, (bad,), 105, 5)
            self.assertEqual(store.open_incidents(), [])
            store.record_cycle(cfg, (bad,), 110, 10)
            self.assertEqual(store.open_incidents()[0][1], "audio_silence")
            self.assertEqual([row[2] for row in store.events()], ["open"])

    def test_unknown_neither_advances_bad_samples_nor_recovers_an_incident(self):
        cfg = config(min_bad=2, unhealthy=0, recovery=2)
        bad = observation(audio=silence(), reasons=("AUDIO_SILENCE",))
        unknown = observation("UNKNOWN", audio=dead_air.DimensionEvidence("UNKNOWN", ("AUDIO_UNKNOWN",)),
                              video=dead_air.DimensionEvidence("UNKNOWN", ("VIDEO_UNKNOWN",)))
        with tempfile.TemporaryDirectory() as td, dead_air.IncidentStore(Path(td) / "state.db") as store:
            store.record_cycle(cfg, (bad,), 100, 0)
            store.record_cycle(cfg, (unknown,), 101, 1)
            self.assertEqual(store.open_incidents(), [])
            store.record_cycle(cfg, (bad,), 102, 2)
            self.assertEqual(len(store.open_incidents()), 1)
            store.record_cycle(cfg, (unknown,), 103, 3)
            self.assertEqual(len(store.open_incidents()), 1)
            self.assertEqual([row[2] for row in store.events()], ["open"])

    def test_unknown_warning_is_delayed_and_deduplicated(self):
        cfg = config(unknown_after=10)
        unknown = observation("UNKNOWN", audio=dead_air.DimensionEvidence("UNKNOWN", ("AUDIO_UNKNOWN",)),
                              video=dead_air.DimensionEvidence("UNKNOWN", ("VIDEO_UNKNOWN",)))
        with tempfile.TemporaryDirectory() as td, dead_air.IncidentStore(Path(td) / "state.db") as store:
            store.record_cycle(cfg, (unknown,), 100, 0)
            store.record_cycle(cfg, (unknown,), 105, 5)
            self.assertEqual(store.open_incidents(), [])
            store.record_cycle(cfg, (unknown,), 110, 10)
            store.record_cycle(cfg, (unknown,), 120, 20)
            self.assertEqual([row[1] for row in store.open_incidents()], ["unknown"])
            self.assertEqual([row[2] for row in store.events()], ["open"])

    def test_confirmed_missing_dimension_is_independent_of_overall_unknown(self):
        cfg = config(min_bad=1, unhealthy=0, unknown_after=600)
        missing = observation(
            "UNKNOWN", audio=dead_air.DimensionEvidence("UNKNOWN", ("AUDIO_MISSING",)),
            video=dead_air.DimensionEvidence("HEALTHY", ("VIDEO_OK",)),
        )
        with tempfile.TemporaryDirectory() as td, dead_air.IncidentStore(Path(td) / "state.db") as store:
            store.record_cycle(cfg, (missing,), 100, 0)
            self.assertEqual([row[1] for row in store.open_incidents()], ["audio_missing"])
        video_missing = observation(
            "UNKNOWN", audio=dead_air.DimensionEvidence("HEALTHY", ("AUDIO_OK",)),
            video=dead_air.DimensionEvidence("UNKNOWN", ("VIDEO_MISSING",)),
        )
        with tempfile.TemporaryDirectory() as td, dead_air.IncidentStore(Path(td) / "state.db") as store:
            store.record_cycle(cfg, (video_missing,), 100, 0)
            self.assertEqual([row[1] for row in store.open_incidents()], ["video_missing"])

    def test_stateful_check_persists_before_returning_and_watch_uses_same_store(self):
        cfg = config(min_bad=1, unhealthy=0)
        bad = observation(audio=silence(), reasons=("AUDIO_SILENCE",))
        with tempfile.TemporaryDirectory() as td, dead_air.IncidentStore(Path(td) / "state.db") as store:
            result = dead_air.check_cycle_stateful(cfg, store, observer=lambda _c, _s: bad)
            self.assertEqual(result[0].status, "UNHEALTHY")
            self.assertEqual([row[2] for row in store.events()], ["open"])
        with tempfile.TemporaryDirectory() as td, dead_air.IncidentStore(Path(td) / "state.db") as store:
            list(dead_air.watch_cycles(cfg, 2, cycle=lambda _c: (bad,), sleeper=lambda _: None, store=store))
            self.assertEqual([row[2] for row in store.events()], ["open"])

    def test_dimension_recovery_is_independent_and_requires_fresh_samples(self):
        cfg = config(min_bad=1, unhealthy=0, recovery=2)
        both = observation(audio=silence(), video=freeze(), reasons=("AUDIO_SILENCE", "VIDEO_FREEZE"))
        audio_bad_video_good = observation(audio=silence(), reasons=("AUDIO_SILENCE",))
        with tempfile.TemporaryDirectory() as td, dead_air.IncidentStore(Path(td) / "state.db") as store:
            store.record_cycle(cfg, (both,), 100, 0)
            self.assertEqual({r[1] for r in store.open_incidents()}, {"audio_silence", "video_freeze"})
            store.record_cycle(cfg, (audio_bad_video_good,), 101, 1)
            self.assertEqual(len(store.open_incidents()), 2)
            store.record_cycle(cfg, (audio_bad_video_good,), 102, 2)
            self.assertEqual({r[1] for r in store.open_incidents()}, {"audio_silence"})
            self.assertEqual([r[2] for r in store.events()].count("recovery"), 1)

    def test_recovery_uses_healthy_dimension_when_other_dimension_is_unknown(self):
        cfg = config(min_bad=1, unhealthy=0, recovery=2)
        both = observation(audio=silence(), video=freeze(), reasons=("AUDIO_SILENCE", "VIDEO_FREEZE"))
        audio_clean = dead_air.DimensionEvidence("HEALTHY", ("AUDIO_DECODED_NO_EVENT",), sampled_s=2, decoded_s=2)
        video_clean = dead_air.DimensionEvidence("HEALTHY", ("VIDEO_DECODED_NO_EVENT",), sampled_s=2, decoded_s=2)
        audio_unknown = dead_air.DimensionEvidence("UNKNOWN", ("AUDIO_UNKNOWN",))
        video_unknown = dead_air.DimensionEvidence("UNKNOWN", ("VIDEO_UNKNOWN",))
        with tempfile.TemporaryDirectory() as td, dead_air.IncidentStore(Path(td) / "state.db") as store:
            store.record_cycle(cfg, (both,), 100, 0)
            store.record_cycle(cfg, (observation("UNKNOWN", audio=audio_clean, video=video_unknown,
                                                 at="2026-10-23T00:00:01Z"),), 101, 1)
            self.assertEqual({row[1] for row in store.open_incidents()}, {"audio_silence", "video_freeze"})
            store.record_cycle(cfg, (observation("UNKNOWN", audio=audio_clean, video=video_unknown,
                                                 at="2026-10-23T00:00:02Z"),), 102, 2)
            self.assertEqual({row[1] for row in store.open_incidents()}, {"video_freeze"})
            store.record_cycle(cfg, (observation("UNKNOWN", audio=audio_unknown, video=video_clean,
                                                 at="2026-10-23T00:00:03Z"),), 103, 3)
            self.assertEqual({row[1] for row in store.open_incidents()}, {"video_freeze"})
            store.record_cycle(cfg, (observation("UNKNOWN", audio=audio_unknown, video=video_clean,
                                                 at="2026-10-23T00:00:04Z"),), 104, 4)
            self.assertEqual(store.open_incidents(), [])
            self.assertEqual([row[2] for row in store.events()].count("recovery"), 2)

    def test_off_air_requires_a_fresh_decoded_dimension_to_recover(self):
        cfg = config(min_bad=1, unhealthy=0, recovery=1)
        off_air = observation("OFF_AIR", audio=dead_air.DimensionEvidence("UNKNOWN", ("NO_MEDIA_SAMPLE_AFTER_OFF_AIR",)),
                              video=dead_air.DimensionEvidence("UNKNOWN", ("NO_MEDIA_SAMPLE_AFTER_OFF_AIR",)))
        no_decode = observation("UNKNOWN", audio=dead_air.DimensionEvidence("HEALTHY", ("AUDIO_DETECTOR_DISABLED",), sampled_s=2, decoded_s=2),
                                video=dead_air.DimensionEvidence("DEGRADED", ("VIDEO_UNVERIFIED_BY_THUMBNAIL",)))
        contradictory_missing = observation("UNKNOWN", audio=dead_air.DimensionEvidence("HEALTHY", ("AUDIO_MISSING",), sampled_s=2, decoded_s=2),
                                             video=dead_air.DimensionEvidence("UNKNOWN", ("VIDEO_UNKNOWN",)))
        stale_decode = observation("UNHEALTHY", audio=dead_air.DimensionEvidence("UNHEALTHY", ("AUDIO_STALE",), sampled_s=2, decoded_s=2),
                                   video=dead_air.DimensionEvidence("UNKNOWN", ("VIDEO_UNKNOWN",)))
        fresh_decode = observation("OFF_AIR", audio=dead_air.DimensionEvidence("HEALTHY", ("AUDIO_DECODED_NO_EVENT",), sampled_s=2, decoded_s=2),
                                   video=dead_air.DimensionEvidence("UNKNOWN", ("VIDEO_MISSING",)))
        with tempfile.TemporaryDirectory() as td, dead_air.IncidentStore(Path(td) / "state.db") as store:
            store.record_cycle(cfg, (off_air,), 100, 0)
            self.assertIn("off_air", {row[1] for row in store.open_incidents()})
            store.record_cycle(cfg, (no_decode,), 101, 1)
            self.assertIn("off_air", {row[1] for row in store.open_incidents()})
            store.record_cycle(cfg, (contradictory_missing,), 102, 2)
            self.assertIn("off_air", {row[1] for row in store.open_incidents()})
            store.record_cycle(cfg, (stale_decode,), 103, 3)
            self.assertIn("off_air", {row[1] for row in store.open_incidents()})
            store.record_cycle(cfg, (fresh_decode,), 104, 4)
            self.assertNotIn("off_air", {row[1] for row in store.open_incidents()})

    def test_off_air_recovers_on_fresh_unhealthy_media_without_resolving_dimension_incidents(self):
        base = config(min_bad=1, unhealthy=0, recovery=2)
        not_expected = dead_air.DimensionEvidence("NOT_APPLICABLE", ("VIDEO_NOT_EXPECTED",))
        audio_only_stream = dataclasses.replace(
            base.streams[0], expect_video=False, detect_freeze=False,
        )
        cases = (
            ("audio_video", base,
             observation("OFF_AIR", audio=dead_air.DimensionEvidence("UNKNOWN", ("NO_MEDIA_SAMPLE_AFTER_OFF_AIR",)),
                         video=dead_air.DimensionEvidence("UNKNOWN", ("NO_MEDIA_SAMPLE_AFTER_OFF_AIR",))),
             observation("UNHEALTHY", audio=silence(), video=freeze(),
                         reasons=("AUDIO_SILENCE", "VIDEO_FREEZE")),
             {"audio_silence", "video_freeze"}),
            ("audio_only", dataclasses.replace(base, streams=(audio_only_stream,)),
             observation("OFF_AIR", audio=dead_air.DimensionEvidence("UNKNOWN", ("NO_MEDIA_SAMPLE_AFTER_OFF_AIR",)),
                         video=not_expected),
             observation("UNHEALTHY", audio=silence(), video=not_expected,
                         reasons=("AUDIO_SILENCE",)),
             {"audio_silence"}),
        )
        for name, cfg, off_air, recovered_media, independent_incidents in cases:
            with self.subTest(stream=name), tempfile.TemporaryDirectory() as td, \
                    dead_air.IncidentStore(Path(td) / "state.db") as store:
                store.record_cycle(cfg, (off_air,), 100, 0)
                self.assertEqual({row[1] for row in store.open_incidents()}, {"off_air"})
                store.record_cycle(cfg, (recovered_media,), 101, 1)
                self.assertEqual({row[1] for row in store.open_incidents()},
                                 {"off_air", *independent_incidents})
                store.record_cycle(cfg, (recovered_media,), 102, 2)
                self.assertEqual({row[1] for row in store.open_incidents()}, independent_incidents)
                recoveries = [row[1] for row in store.events() if row[2] == "recovery"]
                self.assertEqual(recoveries, ["off_air"])

    def test_transition_evidence_survives_restart_and_is_dimension_scoped(self):
        cfg = config(min_bad=2, unhealthy=1, recovery=2)
        bad1 = observation(audio=silence(), reasons=("AUDIO_SILENCE",), at="2026-10-23T00:00:00Z")
        bad2 = observation(audio=silence(), reasons=("AUDIO_SILENCE",), at="2026-10-23T00:00:02Z")
        good1 = observation(audio=dead_air.DimensionEvidence("HEALTHY", ("AUDIO_DECODED_NO_EVENT",),
                                                              (dead_air.Interval(0, 2),), 2, 2),
                            at="2026-10-23T00:00:03Z")
        good2 = observation(audio=dead_air.DimensionEvidence("HEALTHY", ("AUDIO_DECODED_NO_EVENT",),
                                                              (dead_air.Interval(0, 2),), 2, 2),
                            at="2026-10-23T00:00:04Z")
        with tempfile.TemporaryDirectory() as td:
            path = Path(td) / "state.db"
            with dead_air.IncidentStore(path) as store:
                store.record_cycle(cfg, (bad1,), 100, 0)
                store.record_cycle(cfg, (bad2,), 102, 2)
                opened = json.loads(store.events()[0][3])
                self.assertEqual(opened["evidence"]["threshold"]["sample_count"], 2)
                self.assertEqual(opened["evidence"]["threshold"]["span_s"], 2)
                self.assertIn("AUDIO_SILENCE", opened["evidence"]["affected_dimensions"]["audio"]["reason_codes"])
            with dead_air.IncidentStore(path) as store:
                store.record_cycle(cfg, (good1,), 103, 3)
                store.record_cycle(cfg, (good2,), 104, 4)
            with dead_air.IncidentStore(path) as store:
                events = [json.loads(row[3]) for row in store.events()]
            recovery = next(event for event in events if event["transition"] == "recovery")
            evidence = recovery["evidence"]
            self.assertEqual(evidence["threshold"]["sample_count"], 2)
            self.assertEqual([sample["observed_at"] for sample in evidence["samples"]],
                             ["2026-10-23T00:00:03Z", "2026-10-23T00:00:04Z"])
            self.assertEqual(evidence["samples"][0]["dimensions"]["audio"]["decoded_s"], 2)
            serialized = json.dumps(events)
            self.assertNotIn("https://", serialized)
            self.assertNotIn("secret-token", serialized)

    def test_restart_resets_pending_evidence_but_keeps_open_incidents(self):
        cfg = config(min_bad=2, unhealthy=0)
        bad = observation(audio=silence(), reasons=("AUDIO_SILENCE",))
        with tempfile.TemporaryDirectory() as td:
            path = Path(td) / "state.db"
            with dead_air.IncidentStore(path) as store:
                store.record_cycle(cfg, (bad,), 100, 0)
            with dead_air.IncidentStore(path) as store:
                store.record_cycle(cfg, (bad,), 200, 1)
                self.assertEqual(store.open_incidents(), [])
                store.record_cycle(cfg, (bad,), 201, 2)
                self.assertEqual(len(store.open_incidents()), 1)

    def test_clock_jump_and_stale_gap_reset_pending(self):
        cfg = config(min_bad=2, unhealthy=0, max_gap=5)
        bad = observation(audio=silence(), reasons=("AUDIO_SILENCE",))
        with tempfile.TemporaryDirectory() as td, dead_air.IncidentStore(Path(td) / "state.db") as store:
            store.record_cycle(cfg, (bad,), 100, 0)
            store.record_cycle(cfg, (bad,), 200, 1)  # wall-clock jump
            self.assertEqual(store.open_incidents(), [])
            store.record_cycle(cfg, (bad,), 201, 2)
            self.assertEqual(len(store.open_incidents()), 1)
        with tempfile.TemporaryDirectory() as td, dead_air.IncidentStore(Path(td) / "state.db") as store:
            store.record_cycle(cfg, (bad,), 100, 0)
            store.record_cycle(cfg, (bad,), 110, 10)  # stale observation gap
            self.assertEqual(store.open_incidents(), [])
            store.record_cycle(cfg, (bad,), 111, 11)
            self.assertEqual(len(store.open_incidents()), 1)

    def test_corrupt_database_and_stale_lock_are_preserved(self):
        with tempfile.TemporaryDirectory() as td:
            path = Path(td) / "state.db"
            original = b"owner data, not a database"
            path.write_bytes(original)
            with self.assertRaises(dead_air.PersistenceError):
                dead_air.IncidentStore(path)
            self.assertEqual(path.read_bytes(), original)
            path.unlink()
            lock = Path(td) / ".dead-air.lock"
            lock.write_text("stale owner token")
            with self.assertRaises(dead_air.PersistenceError):
                dead_air.IncidentStore(path)
            self.assertEqual(lock.read_text(), "stale owner token")

    def test_persistence_failure_is_explicit(self):
        cfg = config(min_bad=1, unhealthy=0)
        with tempfile.TemporaryDirectory() as td, dead_air.IncidentStore(Path(td) / "state.db") as store:
            store._db.execute("PRAGMA query_only=ON")
            with self.assertRaises(dead_air.PersistenceError):
                store.record_cycle(cfg, (observation(audio=silence()),), 100, 0)


if __name__ == "__main__":
    unittest.main()
