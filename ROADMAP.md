# Roadmap

This page lists candidates for future work. It is not a release promise, and an item here is not implemented or supported. The current project remains a local foreground CLI; consult the README and source for the behavior available today. Use only streams and media that the contributor owns or is authorized to test, and prefer synthetic fixtures for development.

## FUTURE — provider qualification

Qualify source-specific behavior for Twitch, Kick, Owncast, Icecast, and AzuraCast. Treat each as a separate compatibility question: document what can be observed, which conditions are synthetic-testable, what authorization is required, and what failure modes remain unknown. Listing a provider here does not claim support.

## FUTURE — signal and library checks

- Measure EBU R128 loudness drift with synthetic reference material and explicit measurement limits; do not infer the behavior of a live program from a fixture.
- Explore clock OCR using synthetic or explicitly permissioned clips, confidence thresholds, and false-positive reporting. Keep private overlays and account details out of test media.
- Explore radio rotation and stale-library checks against a user-selected local library. Do not upload file names or library contents as part of a report.

## FUTURE — operator views and recovery

- Consider a multi-stream wall/status page only after resource use, accessibility, and the privacy of displayed source names are specified.
- Consider opt-in OBS recovery only with an explicit operator setting, a clear preview of the action, and a safe default that does not restart or alter OBS. Detection alone must not trigger recovery.

## Good first issues

These are intentionally small and can be done without a live stream:

1. Add a fake-clock regression for one documented status transition using a synthetic observation sequence.
2. Add a provider-qualification checklist that separates verified behavior from unknown behavior without claiming a provider is supported.
3. Improve a sample error explanation using a redacted command and a synthetic `example.invalid` source.
4. Add a fixture that checks one boundary in duration or timestamp handling, with expected values stated in the test.
5. Review one contributor example for accidental private-data requests and replace them with synthetic values.

## Community-led cultural work

Any Día de Muertos remembrance or cultural collaboration is future work and must be shaped with the people and communities involved, with their consent and preferred credit. Do not frame it as Halloween or use remembrance as a product decoration. No such collaboration is implied by this roadmap.
