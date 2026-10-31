# Operating Dead Air

## Start with one source

Install from the repository with `python -m pip install .`. Use `.[youtube]`
only when needed. Install FFmpeg and ffprobe separately. Run `dead-air init`,
edit the generated private TOML, then run `validate`, `doctor`, and one `check`.
These commands also work as `python -m deadair COMMAND` or
`python dead_air.py COMMAND` from a checkout.

`doctor` checks local tools and filter execution. It warns about missing alert
destinations and obviously inadequate timing settings. It does not contact
sources, validate notification secrets, or certify a provider.

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

Argparse usage errors also use 2. For finite watch, unhealthy outranks unknown
across all cycles. Continuous watch ends through interruption or a fatal error.

## Inspect a running monitor

```sh
dead-air status --config dead-air.toml
dead-air status --config dead-air.toml --json
```

Status opens SQLite read-only, without sending notifications, sampling a source,
creating a missing database, taking the writer lock, or resetting its counters.
It displays only stream IDs and bounded evidence; source URLs and credentials
are excluded. It is a freshness view, not proof that a process is running.

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
The watch process holds the writer lock; stop it before acknowledging a failure,
then restart it. Status and failure listing remain available while watch is active.

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

## State ownership and upgrades

Keep state on a local filesystem with working OS locks and hard links (for
example NTFS or ext4). Network shares and filesystems without these semantics
are not qualified. Python uses nonblocking `msvcrt.locking` on Windows and
`fcntl.flock` on POSIX. Each database has its own persistent `.lock` file.
The operating system releases ownership when the process exits or crashes;
file existence alone is not an active lock. Never delete these files while any
monitor is running: doing so can create two owners of different inodes.

The directory's `.dead-air.lock` is a persistent compatibility guard containing
`DEAD_AIR_OS_LOCK_V2`. It prevents an old release from bypassing the new locks.
New releases recognize that exact marker and may use separate databases in the
same directory. Guard publication uses a fully written, fsynced temporary inode
and an atomic hard link. A crash can leave an unused `.dead-air-guard-*` temporary
file; after stopping all instances it can be removed. It does not block restart.

An existing legacy guard with other contents is preserved and startup returns
STATE_LOCKED. Stop **all** old/new monitor processes and back up state before
manually removing a confirmed orphaned legacy guard. To roll back to v0.1.0,
stop all instances and remove the v2 compatibility guard first. Never run the
two versions against the same database at once.

State paths and explicit relative tool paths now resolve beside the TOML, rather
than the shell's working directory. Bare tool names still use PATH. When
upgrading, point `state_path` at the existing database intentionally; changing
the location starts a separate monitoring history. The CLI never migrates or
deletes another state file automatically.

## Alerts and privacy

Each destination is opt-in. Supply endpoints and credentials through the named
environment variables; do not commit them. A healthy stream does not prove
delivery works. Failed attempts remain visible in status; terminal failures are
not automatically retried forever. Pending deliveries and unacknowledged
failures are preserved. Review old failures and removed destinations, then
acknowledge resolved delivery incidents so retention can bound database growth.
There is no notification-test command that silently contacts your receivers.

HTTP requests use bounded DNS workers and connect to the checked addresses,
preserving the original HTTP host and HTTPS hostname verification. Redirects
are rechecked. FFmpeg is a separate media transport: its nested playlists,
redirects, DNS, and TLS backend have different behavior. HTTPS capture explicitly
enables peer verification; hostname checking depends on the FFmpeg TLS backend.
An HTTP playlist can reference HTTPS media, so qualify the entire source path
and the installed FFmpeg build. Treat configuration as trusted operator input,
not a hostile URL sandbox. No TLS-disable setting is exposed.

## Shutdown and acceptance

Ctrl+C and SIGTERM unwind owned state and child cleanup. POSIX child process
groups are stopped even when the direct resolver exits first. Windows direct
children are stopped, but escaped descendants are not Job Object contained.
Do not treat this as containment for hostile executables.

Run the full suite with `python -m unittest discover -s tests -v`, with FFmpeg,
ffprobe, and OpenSSL available. The native suite uses synthetic media and owned
loopback listeners. Installed-package checks live in `scripts/smoke_install.py`.
Windows behavior must pass its own CI matrix. A 24-hour soak, provider-specific
authorization/qualification, power loss, sleep/resume, and receipt by a human
remain separate acceptance checks.
