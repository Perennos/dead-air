# Changelog

## 0.3.0

DEAD AIR is now one Rust binary, `dead-air`. The Python implementation is gone;
nothing in the shipped product needs Python.

- Same commands, flags, exit codes (0/1/2/3/130/141/143), TOML keys and on-disk
  state (schema 2; schema 1 read by status and migrated by the first writer), and
  the same `--json` shapes and formatting. The per-stream config fingerprint is byte
  compatible, so stored observations stay fresh across the upgrade.
- Same detection semantics: three independent conditions with their own evidence
  and positive recovery; missing or inconclusive samples are UNKNOWN; thumbnail
  evidence is DEGRADED at best; the 100 ms acquisition prefix and gap rules.
- Same alert rules: stable event ids, at most three attempts at 0/30/120 s with a
  capped Retry-After, at-least-once, each attempt in an owned child process.
- New `incidents` command (incident ids from local state, read-only).
- New optional `[metrics]` table: `watch` serves Prometheus text on
  `127.0.0.1` (per-source condition signals, last-cycle duration, open incidents,
  delivery failures). See [operations](docs/OPERATIONS.md#metrics).
- New optional `[tempo]` table and `explain` command: an advisory Perennos Tempo
  brief about one incident, built from incident metadata only and stored with its
  Tempo receipt id; `status`, `incidents` and the page show it. Alerts never wait
  on Tempo. New `mcp` command (read-only MCP tools) and `tempo status|policy|mcp-config`.
  See [docs/TEMPO.md](docs/TEMPO.md).
- `doctor` adds an `advice` list in plain words and, when Tempo is enabled, a
  loopback Tempo probe (warnings only).
- Installers download a release build and check its SHA-256 against the release's
  `SHA256SUMS`, fall back to `cargo install --locked`, or print the rustup line.
  A release workflow builds Linux (x86_64 and aarch64 musl), Windows (x86_64 MSVC)
  and macOS (arm64 and x86_64) binaries; CI runs fmt, clippy and tests on Linux,
  macOS and Windows.

Behaviour changes a user may notice:

- `python dead_air.py ...`, `python -m deadair` and the Python import API no
  longer exist; call `dead-air`. The installer no longer creates a `.venv` or
  installs yt-dlp; install yt-dlp yourself for YouTube sources.
- Added JSON fields: `status --json` adds `briefs` and an `incident_id` on each
  open incident; `doctor` adds `advice` (and `tempo`/`loopback_tempo_contacted`
  when enabled); the page's state adds incident `id`s and `tempo`. `init` prints
  the same keys in sorted order. Readable `status` lines name the incident id.
- The page's Start button no longer refuses to start because the persistent
  `.dead-air.lock` guard exists (0.2.0 confused that guard with a live owner). It
  now checks the database's OS lock, refuses while any process holds it or while
  an unknown legacy guard is present, and never deletes a lock file. Stop gives the
  watcher three seconds to clean up before forcing it.
- DNS lookups run on bounded worker threads instead of child interpreters, and
  SMTP now connects to the policy-checked addresses (0.2.0 re-resolved the name).
- HTTPS and SMTP trust the operating system's certificate store or
  `SSL_CERT_FILE`, with the Mozilla roots as a fallback; the sender child receives
  `SSL_CERT_FILE`/`SSL_CERT_DIR` when they are set.
- Hostnames must be ASCII (use the `xn--` form for international names), and URL
  hosts may contain only letters, digits, dots, hyphens and underscores.
- Multicast and site-local addresses are treated as non-public, like private ones.
- The GUI binary on Windows is a console program; the shortcut starts it minimized.

Upgrade notes from 0.2.0: stop every Python monitor (and the old page), run
`bash install.sh` or `install.cmd`, update any launcher or service that called
`python dead_air.py` to call `dead-air` with the same arguments, then run
`dead-air status --config dead-air.toml` before `watch`. No state migration is
needed and history, open incidents and pending alerts carry over. To roll back,
stop the Rust monitor, remove any `[tempo]` or `[metrics]` table from the TOML,
and run 0.2.0 against the same database; it ignores the `tempo_briefs` table.

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
