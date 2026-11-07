# Operating Dead Air

## Start with one source

Install the `dead-air` binary with `bash install.sh` (Windows: `install.cmd`), or
build it with `cargo install --locked --path .`. Install FFmpeg and ffprobe
separately, and yt-dlp only for YouTube sources. Run `dead-air init`, edit the
generated private TOML, then run `validate`, `doctor`, and one `check`.

`doctor` checks local tools and filter execution. It warns about missing alert
destinations and obviously inadequate timing settings, and its `advice` list
says in plain words what to fix. It does not contact sources, validate
notification secrets, or certify a provider. When `[tempo]` is enabled it also
probes the loopback Tempo gateway; Tempo problems are warnings and never fail
the core check.

## Read the result

`check` and `watch` show readable summaries. Add `--json` for one flushed JSON
object per cycle, retaining the `observations` array and adding `open_incidents`
and delivery counts. Pending and failed notifications are visible even when
the current sample is healthy. A check exit code describes its observations;
use status to assess persisted incidents and failed deliveries too.

| Command result | Exit code |
| --- | --- |
| Healthy observations, or successful validate/doctor/init | 0 |
| Unhealthy/off-air observations; status also includes open faults or failed delivery history | 1 |
| Unknown/degraded observations; status also includes absent/stale state | 2 |
| Configuration, tool, persistence, or fatal sender-cleanup failure | 3 |
| Ctrl+C | 130 |
| Consumer closed stdout | 141 |
| SIGTERM handled by CLI | 143 |

Usage errors also use 2. For finite watch, unhealthy outranks unknown across all
cycles. Continuous watch ends through interruption or a fatal error.

## Inspect a running monitor

```sh
dead-air status --config dead-air.toml
dead-air status --config dead-air.toml --json
dead-air incidents --config dead-air.toml --open
```

Status opens SQLite read-only, without sending notifications, sampling a source,
creating a missing database, taking the writer lock, or resetting its counters.
It displays only stream IDs, incident IDs and bounded evidence; source URLs and
credentials are excluded. It is a freshness view, not proof that a process is
running. When a Perennos Tempo brief is stored for an open incident, status shows
its first line and receipt id, marked advisory.

Use `dead-air failures --config dead-air.toml` to list terminal delivery failures
and their event and destination IDs. The command is read-only and paginates with
`--limit` and `--offset`. A terminal failure can be acknowledged individually:

```sh
dead-air ack-delivery --config dead-air.toml --event-id EVENT_ID \
  --destination-id DESTINATION_ID --operator "Alex Marzouca"
```

Acknowledgment records the operator and UTC time; it does not retry, resend, or
delete the delivery row. Pending deliveries cannot be acknowledged. An
unacknowledged terminal failure keeps overall status UNHEALTHY; acknowledgment
removes that historical delivery failure from current health, while any open
incident or unhealthy stream still keeps its own health result. Acknowledged
failed deliveries remain available through the configured retention period and
can then be pruned with their resolved incident history.
Schema-2 acknowledgments use a short SQLite transaction that touches only
delivery metadata, so they can run while a watch process is active.
If another SQLite write remains active beyond the bounded wait, acknowledgment
returns an error and may be retried after the writer finishes.
Schema-1 acknowledgment uses the normal exclusive migration path; stop an older
watch process before retrying a migration that was blocked by it. Status and
failure listing remain available while watch is active.

Each latest observation keeps its original observation timestamp. A sample older
than that stream's `max_gap_s`, a clock rollback, or changed source/detector
configuration is UNKNOWN. A long later observation cannot refresh an earlier
sample. Restart resets pending incident and recovery counters while preserving
open incidents, delivery history, and the last snapshot. Older schema-1 state
without snapshots remains readable as UNKNOWN until a new sample is recorded.
Status and failure listing read schema-1 state without mutation. The first
state-writing command migrates schema 1 to schema 2 in one exclusive SQLite
transaction, preserving existing incidents and delivery rows.

## Budget several streams

The cycle remains serial. Each source receives an equal share of the remaining
cycle time, and unused time carries forward. A budget too small for all captures
produces UNKNOWN evidence; it does not skip later sources forever in favor of
the first one. Start with `cycle_budget_s` comfortably above the sum of capture,
resolution, and analysis needs measured on your machine. Split large groups into
separate configurations/databases if necessary. This changes resource use; there
is no claim of 64 simultaneous decodes.

Watch intervals measure cycle start to cycle start. A 60-second interval with a
15-second cycle waits about 45 seconds. An overrun starts the next cycle without
overlap or a catch-up queue. Notifications dispatch after a cycle; the retry
schedule is the earliest eligibility, not a promise of delivery at that second.
Configure `max_gap_s` above the actual observed cadence, or pending evidence
will appropriately reset. Sampled monitoring can miss failures between samples.

## Metrics

A serious 24/7 deployment usually wants its watchdog watched. Add this to the
config and `watch` serves Prometheus text format 0.0.4:

```toml
[metrics]
enabled = true
port = 47825   # binds 127.0.0.1 only: http://127.0.0.1:47825/metrics
```

The endpoint is read-only, answers only `GET /metrics`, checks the `Host`
header (127.0.0.1 or localhost), and sends `Cache-Control: no-store`. Labels are
stream ids, condition names and delivery states only; no URLs, labels, hosts or
destination ids are exposed. A busy port is reported on stderr as
`METRICS_UNAVAILABLE` and never stops monitoring. The GUI's watcher uses the same
config, so it exposes the same endpoint.

| Metric | Meaning |
| --- | --- |
| `deadair_cycles_total` | completed cycles in this process |
| `deadair_last_cycle_duration_seconds` | last cycle, including persistence and alert dispatch (`NaN` before the first) |
| `deadair_last_cycle_timestamp_seconds` | when the last cycle completed |
| `deadair_stream_status{stream,status}` | 1 for each stream's latest observation status; UNKNOWN before a sample |
| `deadair_condition_signal{stream,condition,signal}` | 1 for the latest good/bad/unknown evidence signal per condition |
| `deadair_observation_timestamp_seconds{stream}` | observation time of each stream's latest sample |
| `deadair_incident_open{stream,condition}` and `deadair_open_incidents` | open incidents |
| `deadair_deliveries{status}`, `deadair_deliveries_retrying` | delivery rows by status; pending rows that already failed once |
| `deadair_delivery_failures_unacknowledged` | terminal failures no operator has acknowledged |

Scrape it from a Prometheus or agent on the same machine. A good first alert rule
is `time() - deadair_last_cycle_timestamp_seconds > 3 * deadair_interval_seconds`:
it catches a stuck or dead watcher, which DEAD AIR cannot report about itself.
The endpoint is only as alive as the watch process; an off-host check is still
the only way to notice the machine itself going away.

## State ownership and upgrades

Keep state on a local filesystem with working OS locks and hard links (for
example NTFS or ext4). Network shares and filesystems without these semantics
are not qualified. DEAD AIR takes a nonblocking `LockFileEx` lock on byte 0 on
Windows (the same byte `msvcrt.locking` took in 0.2.0) and `flock` on POSIX, so
the Python 0.2.0 and Rust releases exclude each other. Each database has its own
persistent `.lock` file. The operating system releases ownership when the
process exits or crashes; file existence alone is not an active lock. Never
delete these files while any monitor is running: doing so can create two owners
of different inodes. The GUI's Stop never deletes a lock file either; it stops
its watcher (SIGTERM to the watcher's process group, three seconds to clean up,
then SIGKILL) and lets the OS release the lock.

The directory's `.dead-air.lock` is a persistent compatibility guard containing
`DEAD_AIR_OS_LOCK_V2`. It prevents a 0.1.0 release from bypassing the locks.
Releases since 0.2.0 recognize that exact marker and may use separate databases
in the same directory. Guard publication uses a fully written, fsynced temporary
inode and an atomic hard link. A crash can leave an unused `.dead-air-guard-*`
temporary file; after stopping all instances it can be removed. It does not
block restart.

An existing legacy guard with other contents is preserved and startup returns
STATE_LOCKED. Stop **all** old/new monitor processes and back up state before
manually removing a confirmed orphaned legacy guard. To roll back to v0.1.0,
stop all instances and remove the v2 compatibility guard first. Never run two
versions against the same database at once.

Upgrading from Python 0.2.0 needs no migration: the Rust release uses schema 2
unchanged. Stop every 0.2.0 process, install the binary, run `dead-air status`,
then start `watch`. Rolling back to 0.2.0 also works on the same database; it
ignores the optional `tempo_briefs` table, and the `[tempo]`/`[metrics]` tables
must be removed from the TOML first because 0.2.0 rejects unknown keys.

State paths and explicit relative tool paths resolve beside the TOML, rather
than the shell's working directory. Bare tool names still use PATH. Point
`state_path` at the existing database intentionally; changing the location starts
a separate monitoring history. The CLI never migrates or deletes another state
file automatically.

## Alerts and privacy

Each destination is opt-in. Supply endpoints and credentials through the named
environment variables; do not commit them. A healthy stream does not prove
delivery works. Failed attempts remain visible in status; terminal failures are
not automatically retried forever. Pending deliveries and unacknowledged
failures are preserved. Review old failures and removed destinations, then
acknowledge resolved delivery incidents so retention can bound database growth.
There is no notification-test command that silently contacts your receivers; the
page's test button sends one labelled test event per destination when pressed.

Each attempt runs in an owned child process (`dead-air __notify-worker`). The
child receives the event and the destination's variable *names* on stdin, and
only those named variables (plus `SSL_CERT_FILE`/`SSL_CERT_DIR` when set) in
its environment. The parent stops and reaps it inside the attempt deadline; a
child that cannot be confirmed stopped is a fatal `DELIVERY_CLEANUP_FAILED`.

HTTP and SMTP requests resolve names with bounded DNS lookups (worker threads
with a deadline; a lookup that outlives it is abandoned, at most eight at once)
and connect to the checked addresses, preserving the original HTTP host and
TLS hostname verification. HTTPS and SMTP trust the operating system's
certificate store (or `SSL_CERT_FILE`), falling back to the bundled Mozilla roots
when the system store is empty. Redirects are rechecked. FFmpeg is a separate
media transport: its nested playlists, redirects, DNS, and TLS backend have
different behavior. HTTPS capture explicitly enables peer verification; hostname
checking depends on the FFmpeg TLS backend. An HTTP playlist can reference HTTPS
media, so qualify the entire source path and the installed FFmpeg build. Treat
configuration as trusted operator input, not a hostile URL sandbox. No
TLS-disable setting is exposed.

Perennos Tempo is never in this path. Alerts are deterministic and never wait
on, call, or depend on Tempo. See [TEMPO.md](TEMPO.md).

## Shutdown and acceptance

Ctrl+C and SIGTERM unwind owned state and child cleanup. POSIX child process
groups are stopped even when the direct resolver exits first. Windows direct
children are stopped, but escaped descendants are not Job Object contained.
Do not treat this as containment for hostile executables. On Windows, closing
the console window gives the watcher about four seconds to clean up.

Run the full suite with `cargo test`, with FFmpeg and ffprobe available
(`DEAD_AIR_REQUIRE_MEDIA=1` makes their absence a failure). The native suites use
synthetic media and owned loopback listeners. Windows behavior must pass its own
CI matrix. A 24-hour soak, provider-specific authorization/qualification, power
loss, sleep/resume, and receipt by a human remain separate acceptance checks.
