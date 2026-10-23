# Promo footer for the three public Perennos repos

Paste at the bottom of each `README.md`. Paths assume the three badge files are copied to
`brand/promo-footer/` in that repo. The badge is an image (links do not work inside an `<img>`),
so the links live in the text line under it. One fun line per repo from the full set in
`CREDIT-LINES-20261023.md`; a README is static, so there is no rotation. Every line is plainly a
joke or true, and none says "AI".

Site lines were written from what each site says on 23 Oct 2026:
- github.com/Perennos: the Perennos account (perennos.com is not live yet; switch the link when it is).
- caymanrituals.com: "Cayman Rituals is a Cayman Islands brand of candles, scent, and apparel" (its headline: "Keep the island").
- n4mu.com: **did not resolve in DNS on 23 Oct 2026** (curl, nslookup and a fetch all failed), so it gets its name and link only. Write its line once the site is live, and check the link before publishing.
- marzoucamedical.com: name and link only, as the BRIEF says (medical advertising rules; no claims, no testimonials).

## `Perennos/demon-mode`

```html
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
  <a href="https://github.com/Perennos/perennos-tempo">Perennos Tempo</a> left no crumbs.<br>
  <a href="https://caymanrituals.com">Cayman Rituals</a>, candles, scent and apparel from the Cayman Islands
  · <a href="https://n4mu.com">N4MU</a>
  · <a href="https://marzoucamedical.com">Marzouca Medical</a>
</p>
```

## `Perennos/dead-air`

```html
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
```

## `Perennos/perennos-tempo` (for the v1 coordinator, after v1.0.1)

```html
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
  <a href="https://github.com/Perennos/perennos-tempo">Perennos Tempo</a> would like to thank its mom.<br>
  <a href="https://caymanrituals.com">Cayman Rituals</a>, candles, scent and apparel from the Cayman Islands
  · <a href="https://n4mu.com">N4MU</a>
  · <a href="https://marzoucamedical.com">Marzouca Medical</a>
</p>
```

## Before publishing (each repo)
1. Check every link resolves (n4mu.com especially).
2. Run the Perennos Tempo wording check on the two text lines.
3. The badge coordinates are George Town, Grand Cayman (19.29°N, 81.37°W); change them only if the maker's line changes.
