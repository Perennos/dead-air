# Repository map

| Path | Responsibility |
| --- | --- |
| `deadair/models.py` | Immutable configuration, evidence, and result types |
| `deadair/constants.py` | Version, resource bounds, and protocol/resolver policy |
| `deadair/config.py` | TOML validation and configuration-relative paths |
| `deadair/engine.py` | Bounded processes, HTTP/media observations, scheduling, alerts, CLI |
| `deadair/persistence.py` | SQLite transitions, delivery ledger, writer locks |
| `deadair/status.py` | Read-only snapshots and readable status |
| `deadair/example.toml` | Starter configuration bundled in the installed package |
| `dead_air_gui.py` | Loopback dashboard, stream entry, and owned watcher controls |
| `deadair/gui/` | Dashboard files bundled with the installed package |
| `deadair/brand/` | Small SVG marks served by the packaged dashboard |
| `dead_air.py` | Original script/import compatibility entry point |
| `tests/` | Source, incident, alert, operations, native media and network regressions |
| `scripts/` | Installed-distribution smoke checks |
| `docs/` | Operations, architecture, research, design and implementation records |
| `brand/` | Original artwork, fonts, and licenses |

`python -m deadair` and the installed `dead-air` console script call the same CLI.
The optional dashboard uses the same engine and bundled resources in source and
installed-package runs; it does not carry a second copy of the monitor logic.
The compatibility import aliases the actual engine module so existing embedding
code and test seams still address the functions that execute. Persistence loads
transition helpers only when recording a cycle; importing it alone must work.

The engine is intentionally retained as one orchestration module for this
release. Further separation into media/network/notification services needs an
explicit public interface and fixture-preserving refactor, not mechanical moves
that break monkeypatch seams or child-worker imports.

Do not import the complete application in pure configuration/model modules.
Do not open writable state from a display/status path. Changes to the starter
configuration must update both TOML copies; the operations suite checks equality.
