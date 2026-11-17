# Spike results

This directory holds no committed measurements. Every JSON file here is written by a harness run or a
CI job; `build.json`, `summary.md` and `summary.json` are produced by the workflow.

Numbers come from `.github/workflows/stall-runtime-spike.yml`, which runs on `ubuntu-latest`:

- `build` builds candidate A's vendored tree, candidate B's release binary and the plugin bundles,
  records provenance in `build.json`, and publishes the `vendor`, `stall-b`, `plugins-dist` and
  `build-meta` artifacts.
- `steady`, `idle`, `quiet`, `faults` and `storage` each download those inputs and run
  `scripts/matrix.mjs`, uploading their run JSON per measurement.
- `report` downloads every results artifact, runs `harness/aggregate.mjs` per directory and over the
  combined runs, and appends `summary.md` to the workflow step summary. Everything is kept as the
  `stall-spike-results` artifact for seven days.

Download that artifact from the workflow run rather than committing run output here.
