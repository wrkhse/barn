# Stall runtime spike

This spike compares two ways of building sourcefed's daemon ("stall") under barn. Candidate A is the
real TypeScript daemon (sourcefed v0.5.2) on Node 26, with every outbound HTTP request going over a
Unix socket to a stand-in barn broker. Candidate B is a Rust stall (tokio + hyper) that runs the same
provider JavaScript in QuickJS realms.

The harness synthesises payloads shaped like GitHub's issues list and Jira's enhanced search reply
through a mock upstream — 100 issues per response, sized like the recorded ones and carrying a
realistic field set, with a sliding window that adds one issue per request in changing mode — drives
monitors over `/rpc`, receives events on SSE, and writes one JSON file per measured run. Measurement runs happen only in GitHub Actions on `ubuntu-latest`; the workflow is not
meant to produce comparable numbers on a laptop.

## Build

Node 26 runs the harness and candidate A. Rust stable compiles candidate B; rquickjs needs
`libclang-dev` and `cmake` on Debian/Ubuntu. `npm install` installs the only dev dependency,
esbuild, which bundles the provider plugins.

```sh
npm install
npm run vendor        # clone sourcefed v0.5.2 into .vendor/sourcefed, build it, copy in the candidate A entry
npm run build:plugins # write plugins-dist/github-issues.js, plugins-dist/jira-query.js and the manifest
npm run build         # both of the above
cargo build --release --manifest-path rust/Cargo.toml
```

The harness reads candidate A from `.vendor/sourcefed/.spike/stall-a.mjs`, candidate B from
`rust/target/release/stall-b`, and the plugin bundles from `plugins-dist/`.

## One measured run

Run from this directory. The harness prints one JSON object and writes it to `--out`.

```sh
node harness/run.mjs --candidate ts --scenario steady --monitors 100 --window 45 \
  --chaos none --run-index 1 --out results/one/ts-steady-100-1.json --node \
  --stall-cmd "$PWD/.vendor/sourcefed/.spike/stall-a.mjs"

node harness/run.mjs --candidate rust --scenario steady --monitors 100 --window 45 \
  --chaos none --run-index 1 --out results/one/rust-seq-steady-100-1.json \
  --scheduler seq --stall-cmd "$PWD/rust/target/release/stall-b" \
  --env "STALL_PLUGIN_DIR=$PWD/plugins-dist" --env STALL_SCHEDULER=seq
```

## The matrix

`scripts/matrix.mjs` runs the variants sequentially, interleaving them by run index, and writes one
JSON per run. `--interval` is the monitor poll interval in seconds, forwarded to the harness.
`--env K=V` is forwarded to every variant (used for `STALL_STORE=memory`).

```sh
node scripts/matrix.mjs --variants ts,rust-seq,rust-realm --scenario steady \
  --monitors 100 --runs 10 --interval 15 --window 45 --out results/steady-100
```

Use `--chaos throw` or `--chaos loop` for the fault scenario.

## Aggregate

```sh
node harness/aggregate.mjs --dir results/steady-100 --out results/steady-100/summary.md
```

## Where the numbers come from

`.github/workflows/stall-runtime-spike.yml` runs on pushes to `spike/stall-runtime` and by manual
dispatch (`mode: smoke` for one short run of every scenario plus the 100-monitor ramp, `mode: full`
for everything). The `build` job
records tool versions and bundle sizes in `results/build.json`; its bundle figures cover the
vendored tree's per-package `dist` directories, because sourcefed builds one bundle per package and
provider. The measurement jobs upload their run JSON; the `report` job writes `results/summary.md`
into the workflow step summary and uploads the whole tree as the `stall-spike-results` artifact
(seven-day retention). `results/` is ignored in git, apart from its note, because the numbers belong
to the CI artifacts.
