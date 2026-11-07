# Verification record for 0.3.0 (Rust)

## Verified complete

- Linux x86_64 (this workspace), rustc 1.97.0, FFmpeg 6.1.1 (GnuTLS build):
  `cargo fmt --check` clean; `cargo clippy --all-targets --locked -- -D warnings`
  clean; `DEAD_AIR_REQUIRE_MEDIA=1 cargo test --locked`: **217 tests, 0 failures**
  (44 unit, 173 integration across the 11 suites that run on Linux), about 20 seconds.
- The ported suites pin every behaviour the Python 0.2.0 suite pinned (183 tests
  there), including native lavfi media, a real HLS incident with HTTPS alert retry
  and recovery through owned sender children, FFmpeg rejecting an untrusted
  certificate, rustls refusing a valid chain for the wrong hostname, HTTP
  redirect/limit/off-air cases, sender deadlines against a
  stalled HTTPS collector and a silent SMTP listener, SIGTERM (143), Ctrl+C (130),
  SIGKILL lock release, and the GUI's token, Host and Origin checks.
- State written by the Python 0.2.0 release (`tests/fixtures/`) reads with the
  same `status --json` text apart from the added `briefs` and `incident_id`
  fields, the config fingerprint matches, and a Rust watcher continues that
  history (recovery, delivery rows kept, acknowledgment) on schema 2.
- `cargo +1.85.0 check --all-targets --locked` passes (the declared minimum Rust).
- `scripts/check-windows.sh` (`cargo check --target x86_64-pc-windows-gnu
  --all-targets` with stand-in C compilers for the bundled SQLite and ring) passes
  with no warnings. This type-checks Rust code only; it links nothing.
- Release build (`cargo build --release --locked`, thin LTO, one codegen unit,
  stripped): 6.1 MB for x86_64-unknown-linux-gnu. Cold start of `validate` is about
  1.6 ms (median of 20) against about 105 ms for the Python release on the same
  machine.

Native tests used generated media and bounded loopback HTTP/HLS, HTTPS (a
throwaway test CA), Tempo and SMTP listeners owned by the tests. No public
stream, real alert recipient or real Tempo gateway was contacted.

## Untested

- Windows and macOS execution: only the Windows type-check above ran here. The CI
  matrix (ubuntu, windows and macos runners) is the evidence for those platforms,
  and none of its results are implied by this record.
- The release workflow, the musl and macOS release targets, and the installers'
  real download path against a published release (the installer's download,
  checksum and fallback logic is tested with a fake release folder).
- Real YouTube or other provider qualification, a completed 24-hour soak,
  sleep/resume, power loss and receipt by a human; a real Perennos Tempo gateway
  and a real MCP host.
- Full TLS hostname behavior across FFmpeg backends/nested playlist transports,
  and hostile URL/process containment.
- Filesystems without local lock/hard-link semantics; network shares.

# Verification record for 0.2.0 (Python)

## Verified complete

- Local full discovery: `python -m unittest discover -s tests -q` — **115 tests,
  0 failures**, 36.854 seconds, Python 3.12.14 / Linux / FFmpeg 6.1.1.
- Wheel: `python -m pip wheel --no-deps --no-build-isolation --wheel-dir dist .`.
- Installation into a fresh venv with `--no-deps --no-index`, using that wheel.
- `scripts/smoke_install.py` using the installed interpreter and console command:
  version, non-overwriting init, bundled config, validation, absent-state status,
  standalone package imports, compatibility import and child notification worker.
  It ran outside the checkout and contacted no external service.
- `git diff --check` — no whitespace errors.
- Independent read-only whole-branch review, including synthetic reproductions.
  The reviewer was an available coding agent, **not Argon**.

Native tests used generated media, bounded local HTTP/HLS, self-signed HTTPS and
SMTP listeners owned by the tests. No public stream or real alert recipient was
contacted. The initial baseline was 88 tests with one transient SMTP listener
timing failure; the focused suite and final full suite passed. The latest full
run above is the completion evidence, not that baseline.

## Independent review punch list

| Finding | Resolution and evidence |
| --- | --- |
| An older writer could bypass the new writer lock | Persistent compatibility guard blocks the original O_EXCL protocol; old/foreign contents are preserved; regression passed |
| Cycle completion made an old sample appear fresh | Snapshot freshness derives from the original observation timestamp; long-cycle stale regression passed |
| Huge TOML numeric settings could raise OverflowError | Bounded configuration error and malformed-input regression passed |
| Synchronous DNS could stall the observation loop | System lookups run in bounded owned children; HTTP connects to checked numeric addresses; resolver/connector regressions and native HTTP/TLS suite passed |
| Guard creation could crash before writing its marker | Fsynced temporary inode published by atomic hard link; publication-failure/retry and abrupt-exit regressions passed |

Additional investigation reproduced untrusted HTTPS capture acceptance on the
installed older FFmpeg. Capture now explicitly enables peer verification for
HTTPS inputs; the new negative certificate fixture passed. The first broad flag
application broke HTTP HLS, which was caught by native acceptance; restricting
the option to HTTPS inputs restored the fixture. No failed experimental code is
the basis for the completion claim.

## Untested

- Windows execution in this Linux workspace; the repository CI matrix is the
  source of evidence for Windows/Python 3.11/3.14 results.
- Real YouTube or other provider qualification, a completed 24-hour soak,
  sleep/resume, power loss and receipt by a human.
- Full TLS hostname behavior across FFmpeg backends/nested playlist transports,
  and hostile URL/process containment. Python HTTP and FFmpeg have separate
  acquisition paths; see [operations](OPERATIONS.md).
- Filesystems without local lock/hard-link semantics; network shares.
- `/ponytail` execution and Argon planning: no callable route was discovered.
  The [research handoff](RESEARCH_AND_NEXT_STEPS.md#argon--ponytail-handoff)
  preserves the requested follow-up.

## In progress

GitHub publication is blocked by credentials/connection scope. Direct Git push
reported no username credentials; connected GitHub tree creation and branch
creation each returned HTTP 403 `Resource not accessible by integration`.
No branch, PR, CI run, merge, or release was created. The local commits, exact
patch, complete source, tested wheel and Git bundle are preserved for handoff.
The connection must gain write access to this repository before publication;
no change to ChatGPT's approval preference can grant missing GitHub OAuth/app scope.

No artwork or product UI changed in this patch. The final read-only audit was
domain QC of behavior, packaging, privacy, documentation, attribution and tests.
Future product items are explicitly separated in the research record, with
acceptance gates and no claim that those integrations are installed or working.
