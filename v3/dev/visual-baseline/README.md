<!--
Copyright (c) 2026 BEENTHERE VENTURES, INC.
SPDX-License-Identifier: GPL-3.0-only
-->

# Visual baseline

`v2/` holds screenshots of the v2 Dashboard's mock preview: every page (with
a populated cluster and on first launch), every run state, every dialog, and
every request kind. M3 compares the v3 Web Dashboard against these images.
`v2/manifest.json` lists each image with what it shows.

Regenerate them from the repository's v2 `dashboard/` (its dependencies
installed with `pnpm install` there):

```bash
cd v3
pnpm capture:v2-baseline
```

The script starts the v2 preview on port 1420 if it is not already running,
drives it through its own store and mock backend, and stops the preview
afterwards. It needs Google Chrome; set `BMAC_CHROME` or pass `--chrome` if
it is not at `/usr/bin/google-chrome`. `--only PREFIX` recaptures only the
images whose names start with `PREFIX`, and `--out DIR` writes elsewhere.

The clock, time zone, run IDs, and process IDs are pinned and scrollbars are
hidden, so two captures of the same v2 commit are byte-identical. The mock
has no failing scenario, so `run-failed.png` comes from answering a form
without its values, which makes the mock report a failed run.

## Comparing v3

```bash
cd v3
pnpm compare:visual            # or: node dev/visual-baseline/compare.mjs
```

The script starts the v3 Web Dashboard's mock (`webui`, port 5179), logs in,
drives it into the same states as the v2 images, and captures each under its
v2 name. Pages map to their v3 replacements: dashboard is Overview,
operations is Activity, and config and settings are both Settings. Each
capture is checked twice:

- **Against v2.** It must match `v2/` to within 1% of its pixels, unless
  `intended.json` gives a reason for the difference. Every image in the
  manifest needs one of the two, so a state with no v3 capture needs a
  reason too. A reason on an image that now matches is reported as stale
  (a warning, not a failure).
- **Against the accepted v3 image.** `v3/` holds the images last accepted
  for v3. A capture that drifts more than 0.2% from its accepted image fails
  as CHANGED, so an unnoticed visual change can't hide behind a reason in
  `intended.json`.

`--accept` copies this run's captures into `v3/`; look at the report first.
`--only PREFIX` captures and checks only the images whose names start with
`PREFIX`.
The report, with side-by-side images and the diffs, is written to
`target/visual/report.html`. The script exits non-zero on any failure.

Running jobs come from the mock's scripted scenarios (`webui/src/lib/mock/scenarios.ts`).
The capture pauses them at a fixed number of events (`holdJobs`) and hides
tooltips, so two runs give the same images.
