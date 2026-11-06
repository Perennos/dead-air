# Repository map

DEAD AIR is one Rust binary, `dead-air`. `src/main.rs` only calls `cli::main`; the
library holds the whole product so the integration suites under `tests/` exercise
the same code that ships.

| Path | Responsibility |
| --- | --- |
| `src/cli.rs` | Commands, flags, exit codes, signal-to-exit mapping |
| `src/config.rs` | Strict TOML validation, config-relative paths, the bundled starter |
| `src/model.rs` | Config, evidence and result types; bounded evidence; the config fingerprint |
| `src/constants.rs` | Version, resource bounds and resolver policy |
| `src/interrupt.rs` | Ctrl+C/SIGTERM flag and the unwind that releases owned resources |
| `src/process.rs` | Bounded children: process groups, capped pipes, deadlines, finite cleanup |
| `src/netpolicy.rs` | URL policy, address classification, bounded DNS, private allowlist |
| `src/url.rs` | The subset of Python's `urllib.parse` the policy relies on |
| `src/http.rs`, `src/tls.rs` | Proxy-free HTTP/1.1 client pinned to checked addresses; rustls verification |
| `src/smtp.rs` | SMTP submission over implicit TLS or mandatory STARTTLS |
| `src/media.rs` | ffprobe/FFmpeg evidence: decoded coverage, detector intervals, dimensions, capture |
| `src/observe.rs` | Source resolution, off-air evidence, thumbnail fallback, the serial cycle, incident signals |
| `src/persistence.rs` | SQLite schema 2, transitions, delivery ledger, OS writer locks, legacy guard, acknowledgment, briefs |
| `src/alerts.rs` | Destination payloads, the owned sender worker, dispatch with 0/30/120 s attempts |
| `src/watch.rs` | Serial watch cycles, stateful check, output formatting |
| `src/status.rs` | Read-only snapshots, failures, incident and delivery views |
| `src/doctor.rs` | Local tool checks and plain-words advice |
| `src/gui.rs` | Loopback page server, stream entry, owned watcher, test alerts |
| `src/metrics.rs` | Optional loopback Prometheus endpoint for `watch` |
| `src/tempo.rs` | Optional Perennos Tempo client and `explain` |
| `src/mcp.rs` | Read-only MCP server over stdio |
| `src/pyjson.rs`, `src/timefmt.rs`, `src/random.rs` | Python-compatible JSON text, UTC time, ids and tokens |
| `gui/` | The page's HTML, CSS and JS, embedded at build time |
| `brand/` | Original artwork, fonts and licenses; three marks are embedded for the page |
| `tests/` | Integration suites; `tests/fixtures/` holds state written by the Python 0.2.0 release |
| `scripts/check-windows.sh` | Windows type-check from Linux without a MinGW toolchain |
| `docs/` | Operations, Tempo, architecture, research, design and verification records |

## Design rules

- Everything the network or a child can do is bounded: deadlines on every socket
  operation and child, byte caps on every pipe and body, and an owned path that
  stops and reaps every child, including during Ctrl+C or SIGTERM. A pending
  signal unwinds the command thread so `Drop` releases children, temporary
  captures and the writer lock.
- Display paths never open the writer. `status`, `failures`, `incidents`, the
  GUI and the MCP server read SQLite with read-only connections.
- The on-disk format is the Python 0.2.0 schema 2. JSON written to state, event
  payloads and `--json` output is produced in the Python release's exact text
  form (`src/pyjson.rs`), and the per-stream config fingerprint is byte
  compatible, so upgrades keep their freshness checks.
- The detector core stays deterministic. Perennos Tempo is an optional,
  operator-requested reader of incident metadata and never sits in the alert
  path.
- Changes to the starter configuration happen in `dead-air.example.toml`, which
  `init` and the GUI's first run embed directly.
