# Changelog

## 0.2.0

- Installable `dead-air` command, `python -m deadair`, bundled starter config,
  non-overwriting `init`, and the original script/import compatibility entry point.
- Organized configuration, models, persistence, status, and engine modules.
- Read-only status during watch, original observation age, stale/config-change
  handling, open incidents, and pending/sent/failed delivery counts.
- Malformed persisted observation times stay UNKNOWN without non-finite ages.
- Human-readable monitoring output; `--json` retains the observations array and
  adds incident/delivery diagnostics.
- Config-relative state and explicit tool paths; direct-source doctor no longer
  requires yt-dlp; malformed numeric/type/encoding/port inputs fail cleanly.
- Fair serial stream budgets, start-to-start watch intervals, and independent
  missing-track recovery into silent/frozen media.
- Crash-released per-database OS locks and an atomic legacy compatibility guard.
- Bounded DNS children, checked-address HTTP connections, and explicit HTTPS
  capture certificate verification for older FFmpeg builds.
- SIGTERM cleanup and POSIX descendant cleanup after the direct child exits.
- Added operator documentation, research-backed next steps, installed-package
  checks and Alexander Marzouca contributor credit.
- Added the loopback GUI from the public 0.1.0 line, with read-only status,
  validated stream entry, owned watcher controls, and click-only test alerts.

Upgrade notes: stop existing monitors first, review config-relative paths, keep
`--json` for machine consumers, and read [state ownership](docs/OPERATIONS.md#state-ownership-and-upgrades)
before changing or removing any persistent lock file.
