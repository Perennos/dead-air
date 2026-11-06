# Contributing to DEAD AIR

DEAD AIR is a local, foreground command-line monitor for streams that the operator owns or is authorized to check. Contributions should keep the behavior understandable from one command and safe to reproduce with synthetic data.

## Before opening a change

- Read the project README and the relevant source and tests.
- Keep a change narrow. Explain the user-visible behavior and the boundary it preserves.
- Reproduce failures with fakes or synthetic fixtures. Never require a contributor or reviewer to contact a real stream to verify a change.
- Keep provider-specific support, background operation, and actions that affect a source outside the change unless the issue explicitly authorizes them.

## Private data stays private

Do not commit or attach real configuration files, stream URLs, tokens, authorization headers, private traces, screenshots containing account details, or personal data. Redact command output before posting. A URL being public or playable does not make it yours to monitor or republish. Use clearly synthetic values such as `https://example.invalid/stream` in examples.

If a secret was exposed, remove it from the working copy and ask the owner to rotate it. Deleting a visible line may not remove it from repository history. Report vulnerabilities privately as described in `SECURITY.md`, not in an issue or pull request.

## Local checks

DEAD AIR is a Rust crate (edition 2024, minimum Rust 1.85). Run the same gates CI runs:

```text
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

The native media and loopback suites (`tests/media.rs`, `tests/network.rs`) need FFmpeg and
ffprobe on PATH and are skipped without them; set `DEAD_AIR_REQUIRE_MEDIA=1` to make a missing
tool a failure. They generate synthetic media and talk only to listeners they own. A focused
run is `cargo test --test incidents` (or `alerts`, `observations`, `operations`, `gui`).

On Linux, `scripts/check-windows.sh` type-checks the Windows build without a MinGW toolchain;
real Windows builds and tests run on the windows-latest CI runner.

Tests and examples must use fake clocks, fake senders, and synthetic media/data where applicable. Do not use external resolvers, live endpoints, or unbounded continuous runs as a routine contribution check. If a change needs an integration check, describe the authorization and exact boundary; do not put private endpoint details in the public change. Changes to on-disk state or `--json` output must keep `tests/watch.rs`'s Python 0.2.0 fixture checks passing.

## Pull requests

Include the problem, the smallest change that addresses it, exact local commands and results, and any remaining limitation. State whether the check used fakes or contacted an external service. Keep generated artifacts and unrelated formatting changes out of the diff. For a first contribution, choose a scoped item in `ROADMAP.md` or ask for a synthetic-data task.
