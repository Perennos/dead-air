# README hero snippet: Perennos DEAD AIR

Paste this at the very top of `README.md`. Paths assume the kit lives at `brand/` in the repo root
(`hero.svg`, `hero-dark.svg`, `hero-light.svg` are there). GitHub renders the SVG inside an `<img>`,
so the CSS animation runs on github.com; the loop is 8 s and collapses to the dead-air frame under
`prefers-reduced-motion`.

```html
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
```

Other assets in this folder:

| File | Use |
|---|---|
| `social-preview-1280x640.png` | Repo settings → Social preview (GitHub wants 1280×640). |
| `avatar-512.png` | Org/repo avatar, Discussions, badges. |
| `favicon.svg` | Docs site or status page favicon; simplified for 16–32 px. |
| `mark.svg`, `mark-light.svg` | The logo mark with the colour-bar eyes (live), on dark and on bone. |
| `mark-dead.svg` | The alert state (red eyes), for the alert templates and the ofrenda log. |
| `alt-cut-paper.svg` | The riskier alternative mark; not used unless chosen. |

The log lines inside the hero are an illustration of what an alert looks like, not output from a real stream.
Credits line for the README (the BRIEF asks for it): the calavera, papel picado and cempasúchil come from the
living Día de Muertos tradition; the project invites an artist from the culture to collaborate on the art
(see `brand/NOTES.md`, "Motif sources").
