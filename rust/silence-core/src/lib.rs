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

    fn next_u32(state: &mut u32) -> u32 {
        *state ^= state.wrapping_shl(13);
        *state ^= *state >> 17;
        *state ^= state.wrapping_shl(5);
        *state
    }

    #[test]
    fn matches_the_dead_air_shared_golden_traces() {
        for line in include_str!("../../../tests/data/silence-traces.tsv").lines() {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
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
    }

    #[test]
    fn matches_512_seeded_randomized_transitions() {
        let mut state = 0x0015_5DA1;
        for index in 0..512u32 {
            let now_ms = u64::from(next_u32(&mut state));
            let distance = u64::from(next_u32(&mut state));
            let threshold_ms = if index % 17 == 0 {
                0
            } else {
                u64::from(next_u32(&mut state))
            };
            let selector = next_u32(&mut state);
            let last_signal_ms = if index % 11 == 0 {
                None
            } else if index % 7 == 0 {
                Some(now_ms + distance + 1)
            } else {
                Some(now_ms.saturating_sub(distance))
            };
            let already_quiet = next_u32(&mut state) & 1 != 0;

            let expected = match last_signal_ms {
                Some(last) if threshold_ms > 0 && last <= now_ms => {
                    if now_ms - last >= threshold_ms {
                        if already_quiet {
                            "quiet"
                        } else {
                            "alert"
                        }
                    } else if already_quiet {
                        "recovered"
                    } else {
                        "healthy"
                    }
                }
                _ => "unknown",
            };
            assert_eq!(
                transition(now_ms, last_signal_ms, threshold_ms, already_quiet).as_str(),
                expected,
                "seeded trace {index} ({selector})"
            );
        }
    }
}
