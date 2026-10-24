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

1. Double-click `install.cmd` (macOS/Linux: `bash install.sh`).
2. Click the Dead Air shortcut, add your stream, press **Start watching**, then **Test my alerts**.

The GUI is a local page on 127.0.0.1 only.

## What it does

A stream can be live on the platform and dead on screen. DEAD AIR watches a stream you own and alerts you when the stream goes off-air, freezes, or goes silent, even while the platform still says you're live. Chat is a terrible uptime monitor.

Each cycle it pulls a short sample of the real stream (12 seconds in the example config) and runs ffmpeg's freezedetect and silencedetect on it. yt-dlp resolves YouTube links. A provider thumbnail is a degraded fallback, off in the example.

You need Python 3.11+, ffmpeg and ffprobe, a URL you own, one TOML file, and somewhere to be alerted: ntfy, a Discord webhook, email, or a generic webhook. No stream account, no provider API key, one Python file (the optional GUI adds a second, `dead_air_gui.py`).

The command line offers:

- **validate** — parse the TOML configuration without network access or state writes.
- **doctor** — inspect local binary versions and detector filters.
- **check** — perform one bounded observation cycle.
- **watch** — choose exactly one mode: **--max-cycles N** for a finite run (1–10,000 cycles), or **--continuous** for the authorized foreground 24/7 monitoring path. Both modes run one serial cycle at a time, keep the existing per-cycle and notification bounds, and flush one JSON observation line as each cycle completes. Continuous mode ends on Ctrl+C (exit 130, after owned-child and state cleanup) or a fatal persistence/sender-cleanup failure (exit 3 with a flushed error; sender-cleanup failure is persisted before stopping when the state write succeeds). It does not daemonize or automatically restart.
- **gui** — serve a local page on 127.0.0.1 over the same config and state file (see [GUI](#gui)).

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

## First five minutes

Run these from the project folder, with the interpreter you set up in Install.

**1. Make your private config.** Copy the example. `dead-air.toml` is already in `.gitignore`.

~~~sh
cp dead-air.example.toml dead-air.toml
~~~

~~~powershell
Copy-Item dead-air.example.toml dead-air.toml
~~~

In `[[streams]]`, set `url` to your stream. Set `source` to `"youtube"` for a youtube.com watch or live link or a youtu.be link (this needs yt-dlp, see Install), or to `"direct"` for an http(s) media URL that ffmpeg can read.

At the bottom, uncomment one `[[destinations]]` block:

- `ntfy`, `discord` or `webhook`: keep `id`, `type` and `endpoint_env`. `endpoint_env` is the name of an environment variable. Put the full HTTPS URL in that variable, in the shell that runs DEAD AIR. Never in the TOML.
- `smtp`: keep `id`, `type`, `host`, `port`, `tls`, `sender` and `recipients`. If your server wants a login, add `username_env` and `password_env` together.

Leave every destination commented out and notifications stay off.

**2. Validate.**

~~~sh
python dead_air.py validate --config dead-air.toml
~~~

This parses the file. No network, no state writes. A good file prints `{"ok": true, "stream_count": 1}` for one stream. A bad one prints an `error` and exits 3.

**3. Doctor.**

~~~sh
python dead_air.py doctor --config dead-air.toml
~~~

This looks for ffmpeg, ffprobe and yt-dlp, and checks that ffmpeg has the `freezedetect` and `silencedetect` filters. It prints `"network_contacted": false`. If you skipped yt-dlp because your stream is `direct`, `ok` will still be `false` and the exit code 3. For you, what matters is that `ffmpeg` and `ffprobe` show `"available": true` and `detector_filters_available` is `true`.

**4. One check.**

~~~sh
python dead_air.py check --config dead-air.toml --json
~~~

This is the first step that touches the network: it samples your stream once and prints one line of JSON. The exit code is the short version: 0 means every stream is `HEALTHY`, 1 means `OFF_AIR` or `UNHEALTHY`, 2 means `UNKNOWN` or `DEGRADED`, 3 means the config or the state file is the problem. It also writes to the state file (`state/dead-air.sqlite3` in the example). On the example numbers (`min_bad_samples = 3`, `unhealthy_after_s = 180`), an off-air, freeze or silence alert needs at least three bad samples over at least 180 seconds, so one check will not page you.

**5. The real thing.**

~~~sh
python dead_air.py watch --config dead-air.toml --continuous --json
~~~

This runs in the foreground, one cycle at a time, waits `interval_s` between cycles (60 seconds in the example), and prints one line of JSON per cycle. When a problem lasts long enough, it sends an alert to your destination. After `recovery_samples` good samples (2 in the example), it sends a recovery message. Ctrl+C stops it cleanly (exit 130, after owned-child and state cleanup). It does not daemonize and does not restart itself. Keeping it running, with systemd or Task Scheduler or whatever you like, is your call.

## GUI

`install.cmd` (or `bash install.sh`) makes a `.venv`, installs `requirements.txt` and adds a Dead Air shortcut: Start menu and Desktop on Windows, a `.desktop` launcher on Linux, `Dead Air.command` on the macOS Desktop. It checks for ffmpeg and ffprobe and prints where to get them; it never installs them or Python. `install.cmd /dry-run` and `bash install.sh --dry-run` show the steps and change nothing. Python 3.11 or newer is required.

The shortcut runs `python dead_air.py gui --config dead-air.toml`: a page on 127.0.0.1 only, port 47824 (`--port N` picks another, `--no-browser` skips opening one, `--watch` starts watching at launch; the installer adds it only when `dead-air.toml` already has a stream). Starting it again while it runs just opens the same page. Every run has its own token, which the page sends with each change; requests with another Host or Origin are refused, and the page cannot be framed.

- **Start watching / Stop watching.** The page runs one `watch --continuous` child of its own (output in `dead-air-watch.log` beside the state file, cleared on each start) and says "Watching since" its start time, or that it is not watching. It will not start if another process already holds the state file. **Quit** stops the watcher and the page; closing the tab does not. The page stops itself after 30 minutes without a request unless its watcher is running.
- **Status.** Each stream shows its status, open incidents, last alert and last check, read from the state file without writing to it.
- **Add a stream.** The entry is checked by the same loader as `validate`, then `dead-air.toml` is replaced in one step. If the file does not exist it is created from `dead-air.example.toml`, pointing `yt_dlp` at this folder's `.venv` when yt-dlp is installed there. Destinations are not edited; add them to the TOML as in First five minutes.
- **Test my alerts.** Sends one labelled test alert through each destination, one attempt each. This is the only network action the page takes, and only on that click. Endpoints are read from environment variables, so the shortcut needs them set too, or the test reports `ENDPOINT_UNAVAILABLE`.

A watcher you start yourself (First five minutes, step 5) also works; the page shows its cycles but cannot stop it. The state file comes from `watch.state_path` relative to where each program starts, so start both from the project folder, as the shortcut does.

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

## What it will not do

So nothing surprises you at 3 a.m.

- **Run your stream.** The commands report observations; they do not control a stream, encoder, or OBS.
- **Be for other people's streams.** DEAD AIR is a bounded observer for streams you own or are authorized to monitor.
- **Sign in or borrow your cookies.** DEAD AIR does not import browser cookies or sign in to stream accounts.
- **Count UNKNOWN as HEALTHY.** A missing or inconclusive sample remains **UNKNOWN**, not **HEALTHY**. Remote captures include a 100 ms acquisition prefix followed by the configured full common measurement duration. Startup after that prefix, or missing or gapped decoded content in the measurement window, remains **UNKNOWN**. Use sustained samples and positive recovery before treating a source as restored.
- **Let one good check hide a different problem.** It reports three independent conditions: off-air, frozen picture, and silence. Each condition has its own evidence and recovery path; a successful check in one does not clear another.
- **Treat a thumbnail as proof.** When only a provider thumbnail is available, the result is degraded: a thumbnail cannot establish that the actual stream picture is moving.
- **Promise exactly-once alerts.** Delivery is at-least-once: a receiver may accept a message before a connection fails, so a retry can duplicate it.
- **Be a hostile-URL sandbox.** It trusts operator-owned configuration and local tool paths. Do not feed it untrusted URLs or configurations.
- **Contain every Windows child process.** On Windows, stopping a direct child does not provide a general guarantee that every descendant process is contained or stopped.
- **Vouch for a live source.** No live source has been validated by this README.

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
