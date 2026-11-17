# Spike: sourcefed's stall runtime, TypeScript on Node against Rust

**Status: measured in CI, numbers to follow.** This branch is throwaway spike code. It is never merged into main. It exists to answer one question for the owner: should sourcefed's stall (its daemon under barn) stay TypeScript on Node 26, or be rewritten in Rust with source plugins in QuickJS realms?

## The two candidates

|               | A. TypeScript stall                                                                                                                        | B. Rust stall                                                                                                                                         |
| ------------- | ------------------------------------------------------------------------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------- |
| What it is    | sourcefed's own daemon at v0.5.2 (`e6c1ad4`), unchanged                                                                                    | a new Rust stall, tokio 1.54 + hyper 1.12, built for this spike                                                                                       |
| Node          | /opt/homebrew/opt/node/bin/node v26.13.1 equivalent (Node 26 on the runner)                                                                | none                                                                                                                                                  |
| Providers     | sourcefed's provider modules, in process, no isolation                                                                                     | the _same_ pinned provider code (`pollGithubIssues`, `pollJiraQuery`), esbuilt to one IIFE per source type and run in a QuickJS realm (rquickjs 0.14) |
| Outbound HTTP | `globalThis.fetch` replaced by a broker client: every request becomes a `net.request` JSON-RPC line on the inherited stall protocol socket | the realm's `__stall.http` host function, the same `net.request` lines                                                                                |
| Records       | sourcefed's JsonStore, as today (whole file re-read and re-parsed on every operation)                                                      | a store that deliberately mirrors it, field for field, including the naive re-read                                                                    |
| Scheduler     | sourcefed's 5 s tick, monitors polled one at a time                                                                                        | per-plugin realm threads; `rust-seq` polls one at a time (mirrors A), `rust-realm` gives each source type its own thread                              |
| Watchdog      | none: a synchronous loop in provider code blocks the whole process                                                                         | a per-poll deadline interrupt that poisons and rebuilds the realm                                                                                     |

Both candidates speak the same stall protocol (JSON-RPC 2.0 lines on fd 3) and the same HTTP protocol (`POST /rpc`, `GET /events` SSE with sourcefed's `data: {"type":"event",...}` frames), and both reach the network only through the harness's stand-in broker, exactly as barn will own it.

### Three deliberate deviations from a stock install

- **The GitHub rate limiter is lifted in the vendored copy** (`scripts/vendor.mjs` sets the default and the ceiling to 1,000,000 a minute). The limiter is matching policy that barn's broker budgets will own, and at its shipped 30 a minute a 100-monitor workload is limiter-bound, which hides the runtime this spike measures. The lift is one anchored string replacement, it fails loudly if the pinned source changes, and it applies to candidate A only (candidate B has no limiter).
- **Credentials are placeholders.** The providers refuse to poll without a GitHub token and Jira credentials; both candidates fill dummies, and the token never leaves the process because every request goes over the stall protocol to the loopback mock.
- **`stall.start` carries two extra fields.** Its parameters add an `http` object and a `faults` array, and its result adds `http`; `docs/stall-protocol.md` fixes the parameters as `protocol`, `stall` and `state` and the result as `ready`, `version` and `jobs`. They are spike-only: the spike stall binds its own loopback listener instead of receiving barn's descriptors, and `faults` lets one monitor's upstream throw or loop so fault isolation can be measured. None of it settles B1's `stall.start` fields.

## The workload

- Monitors: 60% `github-issues` (`repo-<i>`, label `atelier:build`), 40% `jira-query` (`project = PROJ<i> ORDER BY key ASC`), each monitor with its own upstream identity so latency can be attributed per monitor.
- Poll interval: 15 s, the shortest sourcefed permits (its default is 60 s). Every figure is therefore also reported per poll and per second, and a run at the real default is four times lighter than what is measured here.
- Upstream: a separate mock-upstream process that synthesises payloads shaped and sized like sourcefed's recorded fixtures — 100 issues per response with the real field set, one extra item carrying `pull_request` so the filter is exercised — and slides a window by one item per request, so each poll after priming sees exactly one new item and one that disappeared. In the quiet scenario the ETag is stable and polls answer 304.
- Client: a fake host subscribes to `GET /events` and acknowledges in batches every 2 s, so queues stay bounded as a real host keeps them.
- Window: 45 s per run (20 s idle, 30 s quiet and fault) after a 2 s warm-up. Startup is measured separately as spawn to first `net.request`.
- Runs: 10 per candidate per size for the steady and idle scenarios, 5 for quiet, faults and the memory-store variant, interleaved A, B(seq), B(realm), A, B(seq), B(realm) ... so slow drift on a shared runner hits all three alike.
- Machine: GitHub Actions `ubuntu-latest` (4 vCPU, 16 GiB). Same machine within a job, so each A-against-B comparison is same-machine; the three sizes run in parallel jobs, so sizes are not compared across machines.
- Noise: <the spread paragraph goes here, from the aggregate output: the median, the min-max range and the widest run-to-run percentage>.

## What is measured, and how

| Figure            | Definition                                                                                                                                                                                                                                                                                                                                        |
| ----------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Startup           | spawn to the first `net.request` reaching the broker (cold, includes loading, listener bind and the first poll)                                                                                                                                                                                                                                   |
| Idle RSS          | median resident memory of the whole process tree over a 20 s window with no monitors                                                                                                                                                                                                                                                              |
| Load RSS          | median, p95 and max resident memory of the process tree, sampled at 1 Hz across the window                                                                                                                                                                                                                                                        |
| CPU per poll      | kernel-reported `utime+stime` of the process tree across the window, divided by completed polls; also as a percentage of one core                                                                                                                                                                                                                 |
| Poll ratio        | completed polls divided by the polls the workload was due (below 1 means the candidate fell behind)                                                                                                                                                                                                                                               |
| Event latency     | for each delivered event: the time from the broker receiving the upstream's answer for that monitor to the harness parsing the SSE frame carrying it. Both stamps come from the harness's own clock, so no cross-process clock skew is involved, and the figure is the stall's post-upstream work: parse, diff, dedupe, persist, queue, SSE write |
| End-to-end        | the same, from the moment the request was written to the stall protocol                                                                                                                                                                                                                                                                           |
| Throughput limit  | the size at which polls fall behind their interval, found by a ramp over 100, 250, 500 and 1000 monitors                                                                                                                                                                                                                                          |
| Fault containment | with one monitor's upstream forced to throw, and then to loop synchronously: whether `stall.health` keeps answering, whether the other monitors keep delivering, and how long until delivery resumes                                                                                                                                              |
| Package size      | candidate A: the vendored build's `dist` bytes plus the published package's packed size plus the Node 26 binary it needs; candidate B: the stripped release binary                                                                                                                                                                                |
| Port cost         | non-blank source lines of the TypeScript that a port has to reproduce, per package, counted from the pinned tree                                                                                                                                                                                                                                  |

## Results

<the tables go here. One table per scenario, each row naming scenario, monitors, candidate, scheduler, chaos and store. Fill from the CI artifacts' summary.md, verbatim, and add the storage-isolated comparison and the ramp table.>

Raw run JSON for every measured run is in the `stall-spike-results` artifact of the workflow run, one file per run, with every figure above plus the machine, the toolchain versions and the stderr tail.

## Caveats

<the caveats go here, at least: the limiter lift; placeholder credentials; loopback mock rather than the real APIs, so no TLS, no real latency and no upstream throttling; the 15 s interval standing in for 60 s; no sandbox, no listener handover and no secret handling, which barn owns and this spike does not measure; the QuickJS realms run the provider code unchanged, so a port would not need to rewrite the providers, only host them; the memory-store variant isolates the cost of sourcefed's JsonStore; `rust-seq` and `rust-realm` bracket the scheduler question; and anything the runs could not measure, named.>

## How to re-run it

```sh
cd spike/stall-runtime
npm install && npm run build          # vendor sourcefed v0.5.2 and bundle the plugins
cargo build --release --manifest-path rust/Cargo.toml
node scripts/matrix.mjs --variants ts,rust-seq,rust-realm --scenario steady \
  --monitors 100 --runs 10 --interval 15 --window 45 --out results/steady-100
node harness/aggregate.mjs --dir results/steady-100 --out results/steady-100/summary.md
```

Or push to this branch: `.github/workflows/stall-runtime-spike.yml` builds both candidates and runs every scenario. `mode: smoke` does one short run of each; `mode: full` does the whole matrix. Nothing in this spike has to run on the owner's Mac.
