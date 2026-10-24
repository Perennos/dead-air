<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="brand/hero-dark.svg">
    <source media="(prefers-color-scheme: light)" srcset="brand/hero-light.svg">
    <img src="brand/hero.svg" width="100%"
         alt="Perennos DEAD AIR. Nobody should find out their stream died from a comment at 3 a.m. A papel picado banner cut as a live waveform flutters, then flatlines; the calavera's colour-bar eyes turn signal red and the alert log records the dead air.">
  </picture>
</p>

<h1 align="center">Perennos DEAD AIR</h1>
<p align="center"><strong>Nobody should find out their stream died from a comment at 3 a.m.</strong><br>
A 24/7 stream watchdog: off-air, frozen picture, silence.</p>

## What it does

DEAD AIR is a bounded observer for streams you own or are authorized to monitor. It samples configured sources and reports three independent conditions: off-air, frozen picture, and silence. Each condition has its own evidence and recovery path; a successful check in one does not clear another.

Use sustained samples and positive recovery before treating a source as restored. A missing or inconclusive sample remains **UNKNOWN**, not **LIVE**. When only a provider thumbnail is available, the result is degraded: a thumbnail cannot establish that the actual stream picture is moving.

Remote captures include a 100 ms acquisition prefix followed by the configured full common measurement duration. Startup after that prefix, or missing or gapped decoded content in the measurement window, remains **UNKNOWN**.

The command line offers:

- **init** — create a private starter configuration without overwriting an existing file.
- **status** — inspect persisted health, stale evidence, incidents and alert delivery without network access or state writes.
- **validate** — parse the TOML configuration without network access or state writes.
- **doctor** — inspect local binary versions and detector filters.
- **check** — perform one bounded observation cycle.
- **watch** — choose exactly one mode: **--max-cycles N** for a finite run (1–10,000 cycles), or **--continuous** for the authorized foreground 24/7 monitoring path. Both modes run one serial cycle at a time, keep the existing per-cycle and notification bounds, and flush one JSON observation line as each cycle completes. Continuous mode ends on Ctrl+C (exit 130, after owned-child and state cleanup) or a fatal persistence/sender-cleanup failure (exit 3 with a flushed error; sender-cleanup failure is persisted before stopping when the state write succeeds). It does not daemonize or automatically restart.

These commands report observations; they do not control a stream, encoder, or OBS. No live source has been validated by this README.

## Get running

Python 3.11+, FFmpeg and ffprobe are required. No API key, account, cloud service,
or mandatory Python dependency is needed. yt-dlp is optional for YouTube.

```sh
git clone https://github.com/Perennos/dead-air.git
cd dead-air
python -m venv .venv
```

Use your environment's interpreter explicitly:

| Platform | Install the core | Installed command |
| --- | --- | --- |
| Windows | `.venv\Scripts\python.exe -m pip install .` | `.venv\Scripts\dead-air.exe` |
| Linux/macOS | `.venv/bin/python -m pip install .` | `.venv/bin/dead-air` |

For YouTube, install `".[youtube]"` instead of `.`. Provider changes and local
JavaScript/EJS requirements still need qualification; the adapter does not sign
in or import browser cookies. The pinned adapter is also listed in requirements.txt.

Install system media tools yourself; the application does not run administrator
commands. Examples are `winget install --id Gyan.FFmpeg.Shared -e` on Windows or
`sudo apt install ffmpeg` on Debian/Ubuntu. Availability depends on your machine.
Confirm both `ffmpeg` and `ffprobe` resolve in a new terminal.

The examples below assume the environment's script directory is on PATH. Use the
full command path from the table otherwise. From a checkout,
`python dead_air.py` or `python -m deadair` can replace `dead-air`.

```sh
dead-air init
# Edit dead-air.toml: replace the synthetic source with one you own/are authorized to monitor.
dead-air validate --config dead-air.toml
dead-air doctor --config dead-air.toml
dead-air check --config dead-air.toml
```

`init` refuses to overwrite a file. `validate` is offline and writes no state.
`doctor` checks local tools and warns about timing or missing alert destinations;
a passing result does not prove source health or alert receipt. The example has
notifications disabled. Configure a destination explicitly if you want alerts.

## Watch and inspect

```sh
dead-air watch --config dead-air.toml --max-cycles 3
dead-air watch --config dead-air.toml --continuous
# In another terminal, with no source traffic or notification side effects:
dead-air status --config dead-air.toml
```

Add `--json` to check/watch/status for machine-readable output. Check and watch
flush one object per cycle with `observations`, `open_incidents`, and `deliveries`.
Default output is a concise readable summary. Status works while the writer is
running, marks stale or changed-source evidence UNKNOWN, and exposes failed alert
delivery. Its snapshot is not proof that the monitor process is still running.

Watch is serial and intervals measure cycle start to cycle start. Each remaining
source receives a share of the remaining cycle budget. Continuous mode stays in
the foreground, does not install a service, and stops on Ctrl+C or SIGTERM with
owned-resource cleanup. No completed 24-hour soak is implied.

Exit codes are 0 healthy, 1 unhealthy/off-air, 2 unknown/degraded, 3 configuration
or operational error, 130 Ctrl+C, and 143 SIGTERM. Status also accounts for open
incidents and failed deliveries. See [operations](docs/OPERATIONS.md) for the full
contract, alert retry timing, state ownership, upgrade/rollback, and limits.

Relative `state_path` and explicit relative tool paths resolve beside the TOML;
bare executable names use PATH. When upgrading from 0.1.0, stop old instances,
review these paths, retain `--json` for machine consumers, and read the lock notes.

## Development and next steps

- [Architecture and file map](docs/ARCHITECTURE.md)
- [Research and prioritized acceptance gates](docs/RESEARCH_AND_NEXT_STEPS.md)
- [Changelog and upgrade notes](CHANGELOG.md)
- [Contributors](CONTRIBUTORS.md), including [@marzouca](https://github.com/marzouca)

Run `python -m unittest discover -s tests -v` with FFmpeg, ffprobe and OpenSSL
available. Tests use synthetic media and owned loopback endpoints. CI covers
Python 3.11/3.14 on Linux and Windows, plus an installed-package smoke check.
Do not infer a platform's latest CI result from this description.

## Privacy and safety

- The core observer needs no stream account or provider API key. Optional notification destinations may need secrets; supply those through environment variables, never in the TOML file, source control, or a support log.
- Public targets are the default. Private and loopback targets are denied unless the operator explicitly opts in and names the exact allowed targets. Python HTTPS verifies certificates and hostnames; HTTPS media capture explicitly enables FFmpeg peer verification. FFmpeg hostname checking depends on its TLS backend; qualify the installed build and nested source path.
- This is not a hostile-URL sandbox. It trusts operator-owned configuration and local tool paths. Do not feed it untrusted URLs or configurations. HTTP uses bounded DNS workers and connects to checked addresses. FFmpeg resolves media, nested playlists and redirects separately; those requests are not pinned by the Python transport.
- Process cleanup has platform limits. On Windows, stopping a direct child does not provide a general guarantee that every descendant process is contained or stopped.
- Local state and event evidence persist across cycles. Protect that directory as operational data; it reveals stream IDs and monitoring history. Temporary captures and operational process arguments may contain private source information.
- Notifications use stable event IDs and at most three delivery attempts, scheduled at 0, 30, and 120 seconds. Delivery is at-least-once: a receiver may accept a message before a connection fails, so a retry can duplicate it. SMTP acceptance or webhook success is protocol evidence, not proof a person read an alert.
- This tool does not take over streams, restart encoders, change broadcast scenes, or operate OBS.

## Cultural and type credits

The hand-built artwork draws on the living Día de Muertos tradition as remembrance and celebration: papel picado, cempasúchil, calaveras, and the ofrenda. These are not costume or horror props. The alert log shown in the hero is an illustration, not output from a monitored stream. DEAD AIR welcomes an artist from the culture to collaborate on the artwork and to correct the project; those corrections should be accepted and credited. The design notes and source context are in [brand/NOTES.md](brand/NOTES.md). No generated image assets were used.

The DEAD AIR artwork uses Unbounded and IBM Plex Mono. The IBM Plex Mono subset is internally renamed **Perennos Mono Subset** because the OFL reserves “Plex” for the original font family. Both are distributed under the [SIL Open Font License 1.1](https://scripts.sil.org/OFL); see the supplied [Unbounded license](brand/licenses/Unbounded-OFL.txt), [IBM Plex Mono license](brand/licenses/IBMPlexMono-OFL.txt), and [NOTICE](NOTICE).

The project source and original artwork are licensed under [Apache License 2.0](LICENSE), subject to the separate font terms in NOTICE.

## Limits

Configuration validation is not a network check. A local doctor result is not proof that a remote source is reachable, that samples represent the actual stream, that notifications reach a human, or that a 24/7 deployment will remain healthy. Thresholds and source behavior vary by provider. Review event evidence and alert delivery on your own authorized streams before relying on the monitor.

<p align="center">
  <a href="https://github.com/Perennos">
    <picture>
      <source media="(prefers-color-scheme: dark)" srcset="brand/promo-footer/footer-dark.svg">
      <source media="(prefers-color-scheme: light)" srcset="brand/promo-footer/footer-light.svg">
      <img src="brand/promo-footer/footer.svg" width="360" height="84" alt="Made in the Cayman Islands by Perennos, 19.29°N 81.37°W">
    </picture>
  </a>
</p>
<p align="center">
  Made in the Cayman Islands by <a href="https://github.com/Perennos">Perennos</a>.
  <a href="https://github.com/Perennos/perennos-tempo">Perennos Tempo</a> has entered the chat.<br>
  <a href="https://caymanrituals.com">Cayman Rituals</a>, candles, scent and apparel from the Cayman Islands
  · <a href="https://n4mu.com">N4MU</a>
  · <a href="https://marzoucamedical.com">Marzouca Medical</a>
</p>
