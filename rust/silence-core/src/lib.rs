//! Quiet-state transitions checked against the public DEAD AIR traces.

#[cfg(feature = "trace-fixtures")]
pub const GOLDEN_TRACES: &str = include_str!("../../../tests/data/silence-traces.tsv");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transition {
    Healthy,
    Alert,
    Quiet,
    Recovered,
    Unknown,
}

impl Transition {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Alert => "alert",
            Self::Quiet => "quiet",
            Self::Recovered => "recovered",
            Self::Unknown => "unknown",
        }
    }
}

/// Evaluate a signal without guessing when its timestamp is absent or in the future.
pub const fn transition(
    now_ms: u64,
    last_signal_ms: Option<u64>,
    threshold_ms: u64,
    already_quiet: bool,
) -> Transition {
    if threshold_ms == 0 {
        return Transition::Unknown;
    }
    let Some(last_signal_ms) = last_signal_ms else {
        return Transition::Unknown;
    };
    if last_signal_ms > now_ms {
        return Transition::Unknown;
    }
    if now_ms - last_signal_ms >= threshold_ms {
        if already_quiet {
            Transition::Quiet
        } else {
            Transition::Alert
        }
    } else if already_quiet {
        Transition::Recovered
    } else {
        Transition::Healthy
    }
}

#[cfg(test)]
mod tests {
    use super::transition;

    fn assert_trace(line: &str) {
        let fields: Vec<_> = line.split('\t').collect();
        assert_eq!(fields.len(), 6, "invalid trace row: {line}");
        let last = if fields[1] == "-" {
            None
        } else {
            Some(fields[1].parse::<u64>().expect("last signal timestamp"))
        };
        let now = fields[2].parse::<u64>().expect("current timestamp");
        let threshold = fields[3].parse::<u64>().expect("quiet threshold");
        let already_quiet = fields[4].parse::<bool>().expect("quiet state");
        assert_eq!(
            transition(now, last, threshold, already_quiet).as_str(),
            fields[5],
            "trace {}",
            fields[0]
        );
    }

    #[test]
    fn matches_the_dead_air_shared_golden_traces() {
        let rows: Vec<_> = include_str!("../../../tests/data/silence-traces.tsv")
            .lines()
            .filter(|line| {
                !line.is_empty() && !line.starts_with('#') && !line.starts_with("seeded-")
            })
            .collect();
        assert_eq!(rows.len(), 8);
        for line in rows {
            assert_trace(line);
        }
    }

    #[test]
    fn matches_512_seeded_randomized_transitions_from_shared_file() {
        let rows: Vec<_> = include_str!("../../../tests/data/silence-traces.tsv")
            .lines()
            .filter(|line| line.starts_with("seeded-"))
            .collect();
        assert_eq!(rows.len(), 512);
        for line in rows {
            assert_trace(line);
        }
    }
}
