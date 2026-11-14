from pathlib import Path
import unittest

from deadair.engine import Interval, _interval_exceeded, silence_transition


def shared_trace_rows():
    trace_file = Path(__file__).parent / "data" / "silence-traces.tsv"
    return [
        line.split("\t")
        for line in trace_file.read_text(encoding="utf-8").splitlines()
        if line and not line.startswith("#")
    ]


class SilenceParityTests(unittest.TestCase):
    def test_shared_golden_traces(self):
        rows = [row for row in shared_trace_rows() if not row[0].startswith("seeded-")]
        self.assertEqual(len(rows), 8)
        for trace_id, last, now, threshold, already, expected in rows:
            actual = silence_transition(
                int(now), None if last == "-" else int(last), int(threshold), already == "true"
            )
            self.assertEqual(actual, expected, trace_id)

    def test_seeded_randomized_transitions_use_rust_generated_traces(self):
        rows = [row for row in shared_trace_rows() if row[0].startswith("seeded-")]
        self.assertEqual(len(rows), 512)
        for trace_id, last, now, threshold, already, expected in rows:
            actual = silence_transition(
                int(now), None if last == "-" else int(last), int(threshold), already == "true"
            )
            self.assertEqual(actual, expected, trace_id)

    def test_dead_air_interval_keeps_the_existing_point_one_second_tolerance(self):
        self.assertFalse(_interval_exceeded(Interval(0.0, 9.899, False), 10.0))
        self.assertTrue(_interval_exceeded(Interval(0.0, 9.9, False), 10.0))
        self.assertTrue(_interval_exceeded(Interval(0.0, 0.0, False), 0.05))


if __name__ == "__main__":
    unittest.main()
