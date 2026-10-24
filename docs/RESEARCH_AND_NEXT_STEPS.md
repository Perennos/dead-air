# Research and next steps

Research date: October 23, 2026. Sources are primary project documentation,
maintainer repositories, and standards material. Recommendations below are
engineering judgments, not verified integration claims. No live streams, paid
services, private files, or media uploads were used.

## What the evidence changes

**Keep the detection core deterministic.** FFmpeg's freeze detector compares
frame differences against a noise floor over time; its silence detector applies
level and duration thresholds. A deliberately static artwork stream can satisfy
the freeze condition without being broken. Preserve the opt-out and tune with
representative material. This is not a task that needs an LLM, GPU, or a Hugging
Face model in the critical path. A learned classifier could add cost and uncertain
failure modes without addressing the measured lifecycle defects.

Sources: [FFmpeg freeze detector](https://ffmpeg.org/ffmpeg-filters.html#freezedetect),
[silence detector](https://ffmpeg.org/ffmpeg-filters.html#silencedetect).

**Qualify the installed media transport, not just its version string.** Current
FFmpeg protocol documentation and older installed binaries differ in their TLS
defaults. The local FFmpeg 6.1.1 help reported `tls_verify` default 0; the new
synthetic HTTPS regression proved it accepted an untrusted certificate without
an explicit setting. Capture now enables verification for HTTPS inputs. FFmpeg
also documents backend-dependent hostname behavior. Nested HLS traffic remains
an important qualification boundary, rather than a claim of hostile-URL isolation.

Sources: [FFmpeg TLS](https://ffmpeg.org/ffmpeg-protocols.html#tls),
[HLS demuxer](https://ffmpeg.org/ffmpeg-formats.html#hls).

**Treat YouTube as an optional adapter.** The yt-dlp maintainers document an EJS
component and supported JavaScript runtime for full YouTube coverage. Disabling
remote component fetching is compatible with installing those pieces locally,
but a lone pinned adapter does not prove full provider support. DEAD AIR should
never quietly import cookies, enable plugins, or download executable components
to make a failed check look healthy. Version pin updates need owned fixtures and
an authorized provider check, not an automatic bump to the newest release.

Source: [yt-dlp dependencies and isolation flags](https://github.com/yt-dlp/yt-dlp#dependencies).

**Separate the reader from the writer.** SQLite's `mode=ro` URI supports a reader
that cannot create or update state. Keep read transactions brief and consistent.
WAL brings additional files and read-only deployment constraints; the current
short-transaction rollback-journal design does not need an unmeasured WAL switch.

Sources: [SQLite URI modes](https://www.sqlite.org/uri.html),
[WAL tradeoffs](https://www.sqlite.org/wal.html).

**Use operating-system ownership, not a PID guess.** Python exposes whole-file
advisory locks through `fcntl` on Unix and byte-range locks through `msvcrt` on
Windows. A dedicated per-database sidecar permits an atomic lock without guessing
whether a recycled PID belongs to the former process. Keep the sidecar inode
stable and test process death. Filesystem qualification still matters.

Sources: [Python fcntl](https://docs.python.org/3/library/fcntl.html#fcntl.flock),
[Python msvcrt](https://docs.python.org/3/library/msvcrt.html#msvcrt.locking).

**Reuse packaging standards.** A standard console entry point and optional
YouTube extra reduce startup friction without adding a mandatory framework.
Installability must be tested from a different working directory, where source
checkout imports cannot mask a missing package or bundled resource.

Source: [PyPA project metadata and scripts](https://packaging.python.org/en/latest/guides/writing-pyproject-toml/).

## Candidate evaluation

| Candidate | Decision | What must be proved before adoption |
| --- | --- | --- |
| [Uptime Kuma](https://github.com/louislam/uptime-kuma) | Prefer an optional external watchdog integration over rebuilding a general uptime platform | A monitor on another host alerts when Dead Air or its machine stops. A local-only second process cannot cover host power/network failure. No default outbound heartbeat. |
| [Apprise](https://github.com/caronc/apprise) | Consider only as an isolated optional notification adapter | Preserve bounded workers, explicit destinations, stable event IDs, TLS, redaction and retry ownership. Audit its dependency footprint and BSD-2-Clause terms before bundling. Keep current core dependency-free. |
| [Icecast statistics](https://www.icecast.org/docs/icecast-trunk/server_stats/) | Good next provider-specific adapter candidate for radio | Handle missing/changing fields in `/status-json.xsl`; server metadata is separate evidence from decoded audio. A reported source connection must not clear silence. |
| [OBS WebSocket protocol](https://github.com/obsproject/obs-websocket/blob/master/docs/generated/protocol.md) | Read-only status correlation first | Encoder output status does not prove audience playback health. Control/restart commands require a separate opt-in action policy and tests, including rejected actions and cooldowns. |
| [EBU loudness guidance](https://tech.ebu.ch/loudness) | Add measured loudness drift after lifecycle qualification | Use momentary/short-term measurements and known test signals. A short sample cannot establish whole-programme integrated loudness. Do not apply a broadcast target to every internet stream. |
| Hugging Face audio/video models | No runtime model selected | First establish a labeled, permissioned failure set and show measurable accuracy benefit versus the current detector. No private media upload or model download is needed for this release. |

Uptime Kuma's published monitor types include HTTP, TCP, DNS and push checks;
that makes it a candidate for detecting the watchdog's absence, not a replacement
for decoded media evidence. Apprise's documented service coverage makes it a
candidate for optional delivery breadth. Neither integration was installed or
tested in this change.

Icecast explicitly warns that statistics variables may appear or disappear.
Treat missing fields conservatively. OBS exposes stream status and control as
different protocol operations; an observer must not gain control implicitly.
EBU describes distinct momentary, short-term and integrated measurement windows;
retain that distinction in any future audio-quality module.

## Implementation order and acceptance gates

1. **Release qualification — In progress.** Keep the new regression suite green on
   Windows and Linux, Python 3.11 and 3.14. Verify wheel installation, module and
   legacy script entry points, local HLS faults/recovery, TLS rejection, and crash
   restart. Then run a 24-hour soak on an explicitly authorized source with measured
   CPU/RAM, cycle duration, child count, sample bytes, and database growth. Do not
   label unattended production operation Verified complete from unit tests alone.
2. **Independent watchdog — Untested.** Add an opt-in heartbeat destination,
   preferably checked off-host. Simulate a killed monitor, a frozen process, host
   network loss, and normal shutdown. Keep heartbeat secrets in environment
   variables. A healthy heartbeat proves execution, not a healthy source.
3. **Alert lifecycle — Untested.** Define retention and operator acknowledgment for
   permanently failed/removed destinations; distinguish historical delivery faults
   from current failures. Test delayed opening alerts, recovery-before-retry,
   deduplication and backlog drain. Add a previewable notification test command
   only with explicit destination selection. No silent paid delivery fallback.
4. **Timing and media qualification — Untested.** Exercise slow HTTP headers,
   redirects, nested HLS hosts, key retrieval, expiring signed URLs, clock changes,
   sleep/resume and resource exhaustion. Keep HTTP and FFmpeg transport boundaries
   explicit. Consider a hardened acquisition worker/proxy only if those fixtures
   justify the complexity. Prove deadline and cleanup behavior on both platforms.
5. **Provider presets — Untested.** Start with owned direct HLS and audio-only
   Icecast fixtures, then qualify one public provider with authorization. Publish a
   matrix of observed, synthetic-only, unsupported, and unknown behavior. Add radio
   and static-artwork presets before marketing arbitrary-provider support.
6. **Operator view — Untested.** Build a local read-only wall from the status
   contract, after defining who can view source names and history. Require fresh
   timestamps, explicit UNKNOWN states, keyboard/screen-reader access and clear
   delivery failures. A public web listener needs a separate authentication design.
7. **Audio quality and recovery — Untested.** Validate loudness drift against known
   references. Keep optional OBS recovery behind an action preview, explicit
   operator policy, cooldown and bounded attempts. Detection must remain useful
   without granting stream-control credentials.

## Argon / ponytail handoff

No callable Argon coding/research agent or `/ponytail` skill was found in the
session's tool, plugin, skill, accessible-file or consented-context discovery.
Argon planning and `/ponytail` execution therefore remain **Untested**. An
independent coding reviewer did inspect this branch; that is not Argon.

When the owner's actual Argon route is available, give it this bounded brief:

> Review `docs/ARCHITECTURE.md`, `docs/OPERATIONS.md`, this research record,
> the tests, and the branch diff against v0.1.0. Challenge the implementation
> order using measured reliability and setup friction. Recommend at most three
> next product changes, each with affected modules, a synthetic failure fixture,
> an acceptance metric, dependency/license/cost impact, and remaining uncertainty.
> Do not upload private streams/configuration, add mandatory cloud inference,
> control OBS, or claim live/provider compatibility from source inspection.

For `/ponytail`, first load the owner's actual workflow definition. Do not infer
its full behavior from the existing source comment using that name. This release
does apply explicit ownership and cleanup to processes, pipes, files, locks,
state, interrupts and failed startup, with regression evidence in the tests.
