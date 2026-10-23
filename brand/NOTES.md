# Brand kit notes: Perennos DEMON MODE and Perennos DEAD AIR (Fable design chat, 23 Oct 2026)

Everything here is hand-built SVG. **No generated images were used** (no ComfyUI, no hosted model), so there is no
image model or image licence to record. Fonts are the only third-party material; see "Type and licences".

Folders: `demon-mode/`, `dead-air/`, `promo-footer/`. `_build/` holds the Python builders and the font subsets
(rebuild instructions at the end). `_renders/` holds the evidence frames for review and is not for the repos.

## How the heroes work on GitHub
- GitHub shows a README SVG inside an `<img>` through its camo proxy: no scripts, no external fetches. So every
  font is embedded as a base64 woff2 data URI, every motion is a CSS keyframe animation, and nothing references
  another file. Each hero is 50–75 KB (limit in the BRIEF: 1 MB).
- The static base state of each hero *is* the final frame (DEMON; dead air). The keyframes override it while they
  run, and `@media (prefers-reduced-motion: reduce)` turns the animations off, which leaves that frame. The social
  preview PNGs are rendered from the same base state (`social-preview.svg` is the hero with the animation block
  removed).
- `<picture>` with `hero-dark.svg` / `hero-light.svg` follows the viewer's GitHub theme; `hero.svg` (= dark) is the
  fallback. `README-hero.md` in each folder has the exact snippet.

## DEMON MODE

### Palette (tokens as used in the SVG `:root`)
| Token | Dark (white-hot) | Light (black-hot) | Role |
|---|---|---|---|
| `--base` | `#0A0A0B` | `#FBF6EC` | ground |
| `--ink` | `#FFF4E0` | `#0A0A0B` | text, reticle |
| `--dim` | bone at 55 % | ink at 58 % | labels, ticks, eyebrow (5.8:1 / 4.8:1) |
| `--oxblood` | `#3B0A0A` | same | LEAN segment, chip |
| `--infrared` | `#E1261C` | same | NORMAL segment; the face below the redline; the light tagline |
| `--ember` | `#FF7A1A` | same | RICH segment; the dark tagline (7.6:1); glow |
| `--hot` | `#FFF4E0` (white-hot) | `#0A0A0B` (black-hot) | DEMON segment, the face at the redline |
| `--blue` | `#2C6BFF` | same | AUSTERE only (the one cold accent) |
| `--pointer` | bone | ink | the needle |
| `--glow` | ember | infrared | bloom behind the mark at DEMON, pointer glow, heat haze |
| `--scan` | bone at 5 % | ink at 4.5 % | 4 px scanline pattern |

The light variant is the thermal camera's *black-hot* convention (hottest level drawn as ink) rather than a faded
copy of the dark one; the four lower levels keep their colours. The tagline swaps ember for infrared on the cream
paper because ember on cream is 2.4:1 (fails); infrared is 4.3:1 at 46 px.

### Type
- Display: **Barlow Condensed** SemiBold 600 (wordmark) and Medium 500 (tagline).
- Mono: **Martian Mono** at wght 500 / wdth 100 (readout values, eyebrow) and wght 400 (labels, small print),
  instanced from the variable font.
- Rule: two families, no italics, tabular numerals on the countdown.

### Mark and motion
- The 😈 rebuilt as single-weight (7 px at 256) line art: slanted eye strokes, a wide bowl grin, two horns with a
  convex outer and concave inner edge, on a HUD reticle (ring, three crosshair stubs, 6° ticks).
- The gauge arc runs between the horns (−150° to −30°), five segments of 24°: AUSTERE blue · LEAN oxblood ·
  NORMAL infrared · RICH ember · DEMON white-hot, over a faint bone track so the dark oxblood level still reads.
- The needle rides *outside* the arc, pointing in, so it never covers the face. At DEMON it is pinned against the
  right horn and overshoots 2° into it (a physical stop, which Hallmark allows for physical motion; UI easing is
  `cubic-bezier(.3,0,.1,1)` everywhere else), then snaps back to the left horn at reset.
- Loop: 12 s. Five stages of 2.2 s (needle, lit segment, RESET IN, LEFT, MODE chip + word change together) then a
  1 s reset flash (00:00:00 / 100 % / RESET). At DEMON: face strokes go white-hot, an ember bloom fades in behind
  the mark (radius 256 at 18 % opacity), the pointer gets a blurred glow, and four blurred haze strands rise above
  the DEMON segment. Scanlines are static at 5 %.
- The countdown and the "left %" are illustrative gauge state, not product claims; `README-hero.md` says so.
- `mark.svg` rests the needle at 52° (end of the DEMON segment, not on the horn) so the static mark stays clean.
- `favicon.svg`: no ticks, 16 px strokes, rounded square ground; legible at 32, a red smudge with horns at 16.
- `alt-isotherm.svg` (riskier, not the locked mark): the face exists only as thermal contour lines, oxblood at the
  edge to white-hot at the core. Risk: at 32 px it reads as a blob.

### Motif sources
- Thermal imaging palettes ("white hot" / "black hot" / "ironbow") from FLIR-style camera overlays; the heat
  scale on near-black is the BRIEF's own direction.
- Tachometer with needle stops and a redline; HUD reticle ticks. No cyberpunk, glitch or clipart devil (rejected
  in the BRIEF and here).

## DEAD AIR

### Palette
| Token | Value | Role |
|---|---|---|
| `--base` | dark `#070707` / light `#F4EFE6` | ground |
| `--text` | dark `#F4EFE6` / light `#070707` | text |
| `--dim` | bone at 60 % / ink at 62 % | eyebrow, timestamps, small print (6.5:1 / 5.6:1) |
| `--red` | `#E10600` | signal red: dead eyes, DEAD, VU peak caps, the alert bullet. Reserved for the alert meaning. |
| `--marigold` | `#F7B32B` | cempasúchil petals, a pennant, the alert-sent bullet |
| `--rosa` | `#F2A1C0` | inner petals, forehead flower, a pennant |
| `--lilac` | `#C7B8EA` | eye-socket dots, a pennant |
| `--mint` | `#A8E6CF` | eye-socket dots, a pennant, the "live" bullet |
| `--bone` / `--paper` | `#F4EFE6` | the skull, teeth, a pennant (dark theme only; on the bone paper the cycle is four colours) |
| `--ink` | `#070707` | outlines, sockets, mouth, nose |

"DEAD" in signal red is 4.1:1 on black and 4.3:1 on bone at 150 px (large text: 3:1 needed).

### Type
- Display: **Unbounded** 700 (DEAD / AIR) and 500 (the tagline), instanced from the variable font.
- Mono: **IBM Plex Mono** 400 (log lines) and 500 (eyebrow), shipped as the renamed subset (see licences).

### Mark and motion
- The calavera is built from broadcast gear, as locked: the eye sockets are NO SIGNAL colour-bar cards in the
  house colours (bone · marigold · mint · lilac · rosa · red, with a dark lower strip like a test card's
  castellations); the teeth are eight VU-meter bars with red peak caps; the crown is nine cempasúchil petals
  whose heights follow a waveform envelope, with rosa inner petals. Traditional calavera decoration is kept small
  and respectful: dot rings around the sockets, an inverted-heart nasal cavity, a five-petal flower on the brow.
- `mark.svg` carries the colour-bar eyes (the live state); `mark-dead.svg` is the alert state with red,
  glowing sockets. `favicon.svg` simplifies to the skull, two three-bar eyes, five teeth, five petals.
- Loop: 8 s. 0–53 %: nine papel picado pennants hang from a sagging string and flutter (rotate ± 2.2° and skew
  about their hanging point, three phase variants), each with a waveform cut in a window; three waveform cuts
  cycle every 0.58 s so the band reads as live. At 53 %: every waveform becomes a flat line, the pennants settle,
  the eyes switch from colour bars to red (breathing 1 → 0.7 → 1), DEAD goes red, and the log swaps from
  "03:02:14 live" to "03:02:44 silence 00:30 · picture frozen" and "03:02:45 alert sent → ntfy · discord · email".
  98.5 %: back to live.
- The log lines are an illustration of an alert, not output from a real stream (`README-hero.md` says so).
- `alt-cut-paper.svg` (riskier, not the locked mark): one papel picado pennant in signal red with the calavera
  present only as the cut-out. Risk: red dominates (red is reserved for the alert) and the eyes blur at 32 px.

### Motif sources (and the respect line the BRIEF asks for)
Día de Muertos is a living tradition (inscribed by UNESCO in 2008 as "Indigenous festivity dedicated to the dead").
The kit draws on three of its objects as remembrance and celebration, never as costume or horror:
- **Papel picado**, the cut tissue-paper banners hung over ofrendas and streets; the craft's best-known centre is
  San Salvador Huixcolotla, Puebla. Here the cut pattern is the stream's waveform; the flatline is the moment
  the banner goes still.
- **Cempasúchil** (*Tagetes erecta*, the Mexican marigold), whose colour and scent are said to guide the dead to
  the ofrenda; its petals are the crown, cut as waveform peaks.
- **The calavera**, from the sugar skulls (*calaveras de azúcar*) placed on ofrendas and from José Guadalupe
  Posada's calavera prints (La Calavera Garbancera, c. 1910, later Diego Rivera's Catrina); the broadcast-gear
  face is the project's own joke on the type, and the sockets, teeth and crown stay cheerful.
- **The ofrenda** is the frame for the alert log: the entries are the streams that went dark, kept rather than
  hidden.
The README credits should invite an artist from the culture to collaborate on the art, and the project should
accept their corrections. Nothing here uses Catholic iconography, costume tropes or horror cues (rejected in the
BRIEF).

## Promo footer
- `promo-footer/footer.svg` (= `footer-dark.svg`), `footer-light.svg`: a 360×84 maker's plate in IBM Plex Mono
  (the renamed subset): MADE IN THE CAYMAN ISLANDS / by Perennos / 19.29°N 81.37°W (George Town, Grand Cayman).
  No flag, no stamp, no sea-turtle; the coordinates are the one ornament and they name a real place. Dark paper is
  GitHub's `#0D1117`, light is white, so the plate sits flat on the README.
- `promo-footer/footer.md`: the text line with the links per repo and the fun line from the full set in
  `CREDIT-LINES-20261023.md` (Demon Mode: "left no crumbs"; Dead Air: "has entered the chat"; Tempo: "would like
  to thank its mom"). caymanrituals.com's line was written from the live site on 23 Oct 2026. **n4mu.com did not
  resolve in DNS that day**, so it carries its name and link only until it is live. marzoucamedical.com: name and
  link only. No "AI" anywhere.

## Type and licences
All four families are under the **SIL Open Font License 1.1** (full texts in `_build/fonts/*-OFL.txt`), fetched
from the `google/fonts` repository on 23 Oct 2026, subset with fontTools 4.64 to printable ASCII plus the few
marks used, and embedded as woff2 data URIs (the OFL allows embedding and bundling).
- Barlow Condensed: Copyright 2017 The Barlow Project Authors (https://github.com/jpt/barlow). No reserved name.
- Martian Mono: Copyright 2021 The Martian Mono Project Authors (https://github.com/evilmartians/mono). No reserved name.
- Unbounded: Copyright 2022 The Unbounded Project Authors (https://github.com/googlefonts/unbounded). No reserved name.
- IBM Plex Mono: Copyright © 2017 IBM Corp. with Reserved Font Name "Plex". A subset is a Modified Version, so the
  shipped subsets are renamed internally to "Perennos Mono Subset" (the CSS alias is `PlexMono`); the OFL text
  ships beside them. Credit IBM Plex Mono in the README's credits with the OFL link.
Source TTF SHA-256 (not shipped; re-download from google/fonts): BarlowCondensed-SemiBold 7b619d14…,
BarlowCondensed-Medium 262bd143…, MartianMono[wdth,wght] c3467843…, Unbounded[wght] 323b511b…,
IBMPlexMono-Regular 6a3412f0…, IBMPlexMono-Medium a9b4c49b….

## Hallmark
Gates were applied while building and the read-only audit was run on the finished files; the punch list is in the
hand-over report (`brand-kits-FINAL.md` in the session's shared notes). The page-level gates (nav, footer archetypes,
mobile widths, sticky, inputs) do not apply to image assets; the README snippets use `width="100%"` so nothing
overflows at 320 px. Stamps and pre-emit scores sit in the first XML comment of every SVG.

## Rebuild
```
cd _build
python mkfonts.py <folder with the source TTFs> fonts      # only if the glyph set changes
python build_demon.py ../demon-mode
python build_deadair.py ../dead-air
python build_footer.py ../promo-footer
```
`svgkit.py` expects the woff2 subsets in `_build/fonts` and, for `text_width`, the source TTFs in a sibling
`fonts-dl` folder (edit `SRC` in `svgkit.py` if they live elsewhere). PNGs were rendered with headless Edge at
device scale 1 (`--window-size=1280,640 --screenshot` on an HTML page holding the SVG in an `<img>`).
