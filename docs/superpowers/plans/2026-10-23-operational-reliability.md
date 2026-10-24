# Dead Air operational reliability implementation plan

> Implement inline using superpowers:executing-plans and test-driven-development;
> request one independent whole-branch review at the end.

**Goal:** Make the local watchdog easier to install, run, and diagnose reliably.
**Architecture:** Preserve the single-module observer. Add an isolated read-only
status module and OS-owned database locks. Reuse bounded evidence and SQLite.
**Tech stack:** Python 3.11+, stdlib, FFmpeg; optional yt-dlp.
**Spec:** ../specs/2026-10-23-operational-reliability-design.md

## Global constraints

No new runtime dependency, paid integration, cloud upload, real source traffic,
stream-control action, or artwork redesign. Keep UNKNOWN evidence conservative.

## Review focus

- Existing schema-1 state must remain readable and its incidents preserved.
- A live writer must never lose its lock to a reader or competing writer.
- Small cycle budgets must not report unobserved sources as healthy.
- Status must not treat a clock rollback or old sample as live evidence.
- JSON diagnostics must not expose configured URLs, labels, credentials, or paths.

## Task 1: Configuration and doctor

- [ ] Add failing regressions for optional resolver, incomplete detector execution,
  malformed destination type/resolver metadata and config-relative paths.
- [ ] Implement bounded validation and only require tools used by configured streams.
- [ ] Run operations and source suites, then commit.

## Task 2: Scheduling and incident independence

- [ ] Add failing tests for per-stream deadlines, start-to-start cadence, overrun,
  and missing-track recovery into frozen/silent media.
- [ ] Implement serial fair budgeting and monotonic cadence; fix recovery signals.
- [ ] Run operations, incident, and source suites, then commit.

## Task 3: Crash-safe persistence and read-only status

- [ ] Add failing tests for lock contention, separate databases, abrupt child death,
  legacy-lock preservation, snapshots, stale/absent/rollback evidence, and delivery counts.
- [ ] Use platform OS file locks. Preserve snapshots across restart without keeping
  untrustworthy pending counters. Add a bounded read-only SQLite status reader.
- [ ] Add human output and status CLI with explicit JSON compatibility tests.
- [ ] Run operations and all existing suites, then commit.

## Task 4: Installation, documentation, and release checks

- [ ] Add pyproject.toml with dead-air entry point and optional YouTube extra.
- [ ] Update examples, README, operations guide, CI package/CLI checks and changelog.
- [ ] Verify isolated installation, validate/doctor/status CLI and full discovery.
- [ ] Obtain independent review, address important findings with regressions,
  commit final results, publish branch and PR, and inspect CI.
