from pathlib import Path
import unittest

from deadair.engine import Interval, _interval_exceeded, silence_transition


class SilenceParityTests(unittest.TestCase):
    def test_shared_golden_traces(self):
        trace_file = Path(__file__).parent / "data" / "silence-traces.tsv"
        rows = [line for line in trace_file.read_text(encoding="utf-8").splitlines() if line and not line.startswith("#")]
        self.assertEqual(len(rows), 8)
        for row in rows:
            trace_id, last, now, threshold, already, expected = row.split("\t")
            actual = silence_transition(
                int(now), None if last == "-" else int(last), int(threshold), already == "true"
            )
            self.assertEqual(actual, expected, trace_id)

    def test_seeded_randomized_transitions_match_the_shared_rule(self):
        state = 0x0155DA1

        def next_u32():
            nonlocal state
            state ^= (state << 13) & 0xFFFFFFFF
            state ^= state >> 17
            state ^= (state << 5) & 0xFFFFFFFF
            return state

        for index in range(512):
            now_ms = next_u32()
            distance = next_u32()
            threshold_ms = 0 if index % 17 == 0 else next_u32()
            selector = next_u32()
            last_signal_ms = (
                None
                if index % 11 == 0
                else now_ms + distance + 1
                if index % 7 == 0
                else max(0, now_ms - distance)
            )
            already_quiet = bool(next_u32() & 1)

            if threshold_ms == 0 or last_signal_ms is None or last_signal_ms > now_ms:
                expected = "unknown"
            elif now_ms - last_signal_ms >= threshold_ms:
                expected = "quiet" if already_quiet else "alert"
            else:
                expected = "recovered" if already_quiet else "healthy"

            actual = silence_transition(
                now_ms, last_signal_ms, threshold_ms, already_quiet
            )
            self.assertEqual(actual, expected, f"seeded trace {index} ({selector})")

    def test_dead_air_interval_keeps_the_existing_point_one_second_tolerance(self):
        self.assertFalse(_interval_exceeded(Interval(0.0, 9.899, False), 10.0))
        self.assertTrue(_interval_exceeded(Interval(0.0, 9.9, False), 10.0))
        self.assertTrue(_interval_exceeded(Interval(0.0, 0.0, False), 0.05))


if __name__ == "__main__":
    unittest.main()
