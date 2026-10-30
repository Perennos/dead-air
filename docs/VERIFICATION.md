# Verification record for 0.2.0

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
