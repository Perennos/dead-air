# Dead Air operational reliability

The authorized goal is to improve the existing product end to end. Keep its
stdlib-first, local, foreground observer architecture and its conservative
evidence model. No account integration, paid API, stream control, or cloud
upload is needed. Preserve the existing artwork.

## Approach

Improve the tested CLI in place. A dashboard or framework rewrite would add a
second operational surface before the monitoring lifecycle is dependable.
The implementation choices below are the builder's decisions within the
user's instruction to edit and improve the repository.

## Required behavior

- Direct sources must pass doctor without the optional YouTube adapter.
  Malformed configuration must produce a bounded error, never a traceback.
  Relative state and explicit tool paths resolve beside the configuration.
- Give each remaining stream a fair share of the remaining cycle deadline.
  Watch intervals measure start to start, with no overlapping or catch-up cycles.
- A missing track recovers when that track is decoded, even if it is now silent
  or frozen. Those other incidents remain independent.
- One writer per database, using an OS lock released on process death. Different
  databases may share a folder. Preserve and reject legacy lock files so an old
  writer cannot be bypassed.
- Persist the latest bounded observation alongside incident evidence. A read-only
  status command works during monitoring, marks stale evidence UNKNOWN, reports
  open incidents and destination delivery counts, and never exposes URLs/secrets.
  Opening status must not create files or reset incident counters.
- Default monitoring output is concise and human readable; --json preserves the
  machine-readable observation envelope and adds incident/delivery diagnostics.
- Expose a pip-installable dead-air command with no mandatory Python dependency.
  Document the actual startup path, exit codes, lock recovery, and operating limits.

## Verification

Use fake clocks for scheduling, synthetic observations for transitions and status,
and an owned child process for crash-lock recovery. Run existing FFmpeg/HTTP/TLS
fixtures locally; no live stream or real notification destination is necessary.
Perform a fresh independent whole-diff review before publishing a branch and PR.
Windows behavior is verified by CI, not inferred from this Linux workspace.
