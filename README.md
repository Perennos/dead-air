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

- **validate** — parse the TOML configuration without network access or state writes.
- **doctor** — inspect local binary versions and detector filters.
- **check** — perform one bounded observation cycle.
- **watch** — choose exactly one mode: **--max-cycles N** for a finite run (1–10,000 cycles), or **--continuous** for the authorized foreground 24/7 monitoring path. Both modes run one serial cycle at a time, keep the existing per-cycle and notification bounds, and flush one JSON observation line as each cycle completes. Continuous mode ends on Ctrl+C (exit 130, after owned-child and state cleanup) or a fatal persistence/sender-cleanup failure (exit 3 with a flushed error; sender-cleanup failure is persisted before stopping when the state write succeeds). It does not daemonize or automatically restart.

These commands report observations; they do not control a stream, encoder, or OBS. No live source has been validated by this README.

## Install

Requirements: Python 3.11 or newer, FFmpeg and ffprobe available on PATH. The program does not install system binaries or run an administrator command for you.

Clone into a clean folder:

~~~sh
git clone https://github.com/Perennos/dead-air.git
cd dead-air
~~~

Install FFmpeg yourself before running observations. On Windows, one WinGet option is:

~~~powershell
winget install --id Gyan.FFmpeg.Shared -e
~~~

On Debian or Ubuntu:

~~~sh
sudo apt install ffmpeg
~~~

On Fedora, the package must be available from your enabled repositories:

~~~sh
sudo dnf install ffmpeg
~~~

Package availability and install permissions depend on your machine and repositories. After installation, make sure both ffmpeg and ffprobe resolve from a new terminal.

Create an isolated Python environment. Install the optional pinned yt-dlp adapter from `requirements.txt` only if you configure a supported YouTube source. Use the explicit virtual-environment interpreter; shell activation is not required. Confirm it meets the documented Python 3.11+ requirement.

~~~powershell
py -3 -m venv .venv
.venv\Scripts\python.exe --version
.venv\Scripts\python.exe -m pip install --disable-pip-version-check --index-url https://pypi.org/simple -r requirements.txt
~~~

On Linux or macOS, use `python3` with Python 3.11 or newer and its explicit virtual-environment interpreter:

~~~sh
python3 -m venv .venv
.venv/bin/python --version
.venv/bin/python -m pip install --disable-pip-version-check --index-url https://pypi.org/simple -r requirements.txt
~~~

For the command examples below, use `.venv\Scripts\python.exe` on Windows or `.venv/bin/python` on Linux/macOS when the optional adapter is needed.

The example's `yt_dlp = "yt-dlp"` uses PATH lookup. For a YouTube source, replace it in your private copy of the configuration with the absolute installed console-script path in that virtual environment. Replace the project-path placeholder with the actual absolute path:

~~~toml
# Linux/macOS
yt_dlp = "/path/to/project/.venv/bin/yt-dlp"

# Windows
yt_dlp = "C:\\path\\to\\project\\.venv\\Scripts\\yt-dlp.exe"
~~~

A relative `tools.yt_dlp` path is resolved from DEAD AIR's process working directory, not from the configuration file's directory. Alternatively, prepend `.venv/bin` or `.venv\Scripts` to `PATH` only in the shell/process that starts DEAD AIR; this does not require activation and must not change user or system `PATH`.

yt-dlp is used only for supported YouTube sources. Provider changes can break extraction, and some formats may require a compatible JavaScript runtime. DEAD AIR does not import browser cookies or sign in to stream accounts. A public URL is not a promise that a provider will expose usable media.

## First check

The example is safe for a configuration smoke check:

~~~sh
python dead_air.py validate --config dead-air.example.toml
~~~

This only parses the example TOML. It does not check FFmpeg, contact a source, capture media, send a notification, or prove production readiness. Use doctor for local binary and detector checks. Copy the example to a private configuration file, add only authorized targets, and keep private endpoints out of commits and issue reports. See the command help before running check or a finite watch cycle:

~~~sh
python dead_air.py --help
python dead_air.py check --help
python dead_air.py watch --help
~~~

Run either bounded or continuous watch with one explicit mode:

~~~sh
python dead_air.py watch --config dead-air.toml --max-cycles 3 --json
python dead_air.py watch --config dead-air.toml --continuous --json
~~~

Continuous mode is the foreground path for long-running monitoring; it is not evidence of a completed 24-hour soak or production readiness. An observation can make network requests to every configured target. Start with one check and a source you own or have permission to monitor. Review the configuration before using watch.

## Privacy and safety

- The core observer needs no stream account or provider API key. Optional notification destinations may need secrets; supply those through environment variables, never in the TOML file, source control, or a support log.
- Public targets are the default. Private and loopback targets are denied unless the operator explicitly opts in and names the exact allowed targets. TLS certificate verification remains enabled.
- This is not a hostile-URL sandbox. It trusts operator-owned configuration and local tool paths. Do not feed it untrusted URLs or configurations. DNS checks and socket limits reduce exposure but do not pin DNS results to the eventual connection.
- Process cleanup has platform limits. On Windows, stopping a direct child does not provide a general guarantee that every descendant process is contained or stopped.
- Local state and event evidence persist across cycles. Protect that directory as operational data; it can reveal source addresses and monitoring history.
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
