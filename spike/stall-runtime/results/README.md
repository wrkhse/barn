# Spike results

This directory holds no committed measurements. Every JSON file here is written by a harness run or a
CI job; `build.json`, `summary.md` and `summary.json` are produced by the workflow.

Numbers come from `.github/workflows/stall-runtime-spike.yml`, which runs on `ubuntu-latest`:

- `build` builds candidate A's vendored tree, candidate B's release binary and the plugin bundles,
  records provenance in `build.json`, and publishes the `vendor`, `stall-b`, `plugins-dist` and
  `build-meta` artifacts.
- `steady` writes `results/steady-10`, `results/steady-100` and `results/steady-500`.
- `idle` writes `results/idle` with zero monitors.
- `quiet` writes `results/quiet`, 100 monitors against a stable upstream.
- `faults` writes `results/fault-throw` and `results/fault-loop`, 20 monitors with one upstream
  forced to throw and then to loop.
- `storage` writes `results/steady-mem-100` and `results/steady-mem-500`: the storage-isolated
  variant at both sizes.
- `ramp` writes `results/ramp-100`, `results/ramp-250`, `results/ramp-500` and
  `results/ramp-1000`, one window per size.
- `report` downloads every results artifact, runs `harness/aggregate.mjs` per directory and over
  the combined runs, and appends `summary.md` to the workflow step summary. Everything is kept as
  the `stall-spike-results` artifact for seven days.

A manual dispatch with `mode: smoke` measures one short run of each destination — steady at 10
monitors only, and the 100-monitor ramp — before the same `report` job runs. `mode: full`, the
default for a push, runs the whole matrix, including the 500-monitor storage run.

Download that artifact from the workflow run rather than committing run output here.
