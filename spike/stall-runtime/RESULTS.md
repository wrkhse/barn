# Spike: sourcefed's stall runtime, TypeScript on Node against Rust

**Status: measured.** Every figure in the tables below is a median from the aggregate of the run JSONs that `.github/workflows/stall-runtime-spike.yml` produced on GitHub Actions `ubuntu-latest`; the raw JSON is in the `stall-spike-results` artifact of that run. The figures are from workflow run 44119518251 (workflow "Stall runtime spike") on commit `9830bae`, which measured all three candidates with the same harness: 10 runs per group, 5 for the storage-isolated and fault groups, and 1 per ramp size, interleaved A, B (sequential), B (one realm per type), C. This branch is throwaway spike code. It is never merged into main. It exists to answer one question for the owner: should sourcefed's stall (its daemon under barn) stay TypeScript on Node 26, or be rewritten in Rust — and if in Rust, should its plugins run in QuickJS or in djs?

## The three candidates

|               | A. TypeScript stall                                                                                                                        | B. Rust stall, QuickJS                                                                                                                                | C. Rust stall, djs                                                                                                       |
| ------------- | ------------------------------------------------------------------------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------ |
| What it is    | sourcefed's own daemon at v0.5.2 (`e6c1ad4`), unchanged                                                                                    | a new Rust stall, tokio 1.54 + hyper 1.12, built for this spike                                                                                       | the same Rust stall, compiled with the `engine-djs` feature: one crate, one engine per binary                            |
| Harness name  | `ts`                                                                                                                                       | `rust-seq` and `rust-realm`                                                                                                                           | `rust-djs`                                                                                                               |
| Node          | /opt/homebrew/opt/node/bin/node v26.13.1 equivalent (Node 26 on the runner)                                                                | none                                                                                                                                                  | none                                                                                                                     |
| Providers     | sourcefed's provider modules, in process, no isolation                                                                                     | the _same_ pinned provider code (`pollGithubIssues`, `pollJiraQuery`), esbuilt to one IIFE per source type and run in a QuickJS realm (rquickjs 0.14) | the same bundles, compiled into a djs snapshot (`djs-embed` 0.1.1, pinned by commit) and instantiated for each poll path |
| Engine        | V8, through Node 26                                                                                                                        | rquickjs 0.14 (QuickJS), one realm per plugin type                                                                                                    | djs, one snapshot per plugin type built once and one instance per poll path                                              |
| Outbound HTTP | `globalThis.fetch` replaced by a broker client: every request becomes a `net.request` JSON-RPC line on the inherited stall protocol socket | the realm's `__stall.http` host function, the same `net.request` lines                                                                                | the same host function, declared in the snapshot and answered from the step loop                                         |
| Records       | sourcefed's JsonStore, as today (whole file re-read and re-parsed on every operation)                                                      | a store that deliberately mirrors it, field for field, including the naive re-read                                                                    | the same store                                                                                                           |
| Scheduler     | sourcefed's 5 s tick, monitors polled one at a time                                                                                        | per-plugin realm threads; `rust-seq` polls one at a time (mirrors A), `rust-realm` gives each source type its own thread                              | the same, one realm per source type                                                                                      |
| Watchdog      | none: a synchronous loop in provider code blocks the whole process                                                                         | a per-poll deadline interrupt that poisons and rebuilds the realm                                                                                     | djs's own fuel, memory and wall-clock budgets; a runaway poll is recovered by resetting the instance from its snapshot   |
| Realm build   | —                                                                                                                                          | prelude and bundle evaluated into a fresh context: 4.3 ms at 100 monitors, 3.3 at 500                                                                 | snapshot built once and instantiated: 5.0 ms at 100 monitors, 4.4 at 500                                                 |
| Binary        | Node 26 plus the vendored tree: a 6.07 MB tarball and a 77 MB Node install                                                                 | `stall-b`, stripped: 3,048,824 bytes                                                                                                                  | `stall-c`, stripped: 3,625,272 bytes                                                                                     |

All three speak the same stall protocol (JSON-RPC 2.0 lines on fd 3) and the same HTTP protocol (`POST /rpc`, `GET /events` SSE with sourcefed's `data: {"type":"event",...}` frames), and all three reach the network only through the harness's stand-in broker, exactly as barn will own it.

Candidates B and C differ only in the engine: same scheduler, same store, same broker, same prelude, same bundles.

### One realm interface, two engines

`src/engine.rs` holds one trait, `Engine::poll(input) -> envelope`, with an rquickjs implementation and a djs implementation behind it, chosen by cargo feature: `engine-rquickjs` (the default) or `engine-djs`. Each binary contains exactly one engine — `src/lib.rs` refuses to compile with neither or with both — and both build from the same crate, which is why CI builds `stall-b` and `stall-c` from one source tree. `plugin.rs` builds a poll's input, schedules the poll and reads its envelope, and never learns which engine it has.

The same `src/prelude.js` serves both engines. Its `http` call is now `async` and awaits the host function, which is the shape the pinned providers already await: under rquickjs the host call is a Rust closure that blocks the realm thread and returns a string, and under djs it is a promise the host settles. Each engine logs its realm build and reset times — `[stall-b] engine <engine> realm-build <plugin> <ms>` and `... realm-reset ...` — which the harness records per run, and those lines are where the two realm columns in every table come from.

### What djs changes, and what it risks

What djs could not run: nothing of the two providers. Two tests in the spike run the real bundles (`plugins-dist/github-issues.js` and `jira-query.js`) through djs and prove priming, exactly one `item.matched` on the second poll, the `if-none-match` request, and the 304 branch being served; and the same for `jira-query`'s search. What djs does not have, and would need a shim for, is `Intl` (the schedule provider formats time zones, as it would under rquickjs), `eval` and `new Function`, `SharedArrayBuffer` and `Atomics`, workers, sloppy mode, `with`, and direct `eval`. Its TypeScript erasure means a plugin needs no transpile step: a test runs a TypeScript source with an interface, a generic and casts and gets the expected value back. It does not remove bundling: the pinned provider's own `poll.ts`, loaded as unbundled TypeScript with an empty loader, fails at build with `Error::Load`, "cannot find module 'valibot' imported by 'poll.ts'", and the same would follow for `@sourcefed/core` and the relative `.ts` modules, which is why the spike still ships one esbuild bundle per plugin and why djs replaces the transpile step, not the bundling step.

The async model fit: our broker call is asynchronous. With rquickjs the host function is a Rust closure that blocks the realm thread and returns a value, and the engine is driven by draining its promise job queue (`Promise::finish`). With djs the host functions are declared in the snapshot and the poll is a step machine: `Instance::start`, then a loop on `Instance::step` that answers `Step::Wait` calls with `Instance::resolve`. The spike's honest verdict is that djs's model is the better fit for this boundary, because the host call is explicit and answerable later — no closure captured into the engine, no hand-drained microtask queue, no `unsafe impl Send` to move a realm between threads — the deadline is a budget checked between slices rather than an interrupt handler that has to be armed and disarmed around every poll, and after a runaway poll the realm comes back with a snapshot-level reset. The cost is that the host must drive the loop and that host functions must all exist before the snapshot is built. Neither engine solves this: while a poll is blocked inside our broker call, the realm thread is blocked, so a slow upstream still costs a poll slot rather than being suspended.

Risk: `djs-embed` is `publish = false`, so it can only come from the repository, and the dependency is a git pin to one commit (`0e58f62c80252fc3f4156ab7a3744746434ed860`, "docs: mark the index and introduction as experimental", 2026-11-08). The crates.io-free dependency tree is `djs-core`, `djs-libm`, `djs-prelude` and `djs-lzrc` and nothing else. The repository was created on 2026-11-01, has one contributor (mitsuhiko), 2,389 commits, one tag (`v0.1.0`) and no GitHub releases; `djs-embed` is 0.1.1 and its own CHANGES says a minor bump may break the API. The engine declares `#![cfg_attr(not(feature = "unsafe-heap"), forbid(unsafe_code))]`, so in the configuration we build there is no `unsafe` in `djs-core` at all; counted across the crates we link, `unsafe` appears 37 times in `djs-libm` (the ported musl math, including x86 FMA detection), 0 in `djs-embed`, 0 in `djs-prelude` and 0 in `djs-lzrc`, and 146 times in `djs-wasm`, which we do not link. It builds on our pinned toolchain: rustc 1.101.0-nightly (080cc2714 2026-10-11) compiled it in CI, and it also builds on stable 1.96 in 17 seconds. Its primary target is wasm32 and its native path is secondary. The spike adds exactly one `unsafe` to its own code, an `unsafe impl Send` for the rquickjs realm, because rquickjs's `Runtime` is only `Send` behind its `parallel` feature; the djs engine needs none. djs is a young, single-upstream, unpublished crate, so adopting it in `barn-plugin-host` is a pinning-and-tracking commitment, not a dependency you simply update.

### Three deliberate deviations from a stock install

- **The GitHub rate limiter is lifted in the vendored copy** (`scripts/vendor.mjs` sets the default and the ceiling to 1,000,000 a minute). The limiter is matching policy that barn's broker budgets will own, and at its shipped 30 a minute a 100-monitor workload is limiter-bound, which hides the runtime this spike measures. The lift is one anchored string replacement, it fails loudly if the pinned source changes, and it applies to candidate A only (candidates B and C have no limiter).
- **Credentials are placeholders.** The providers refuse to poll without a GitHub token and Jira credentials; all three fill dummies, and the token never leaves the process because every request goes over the stall protocol to the loopback mock.
- **`stall.start` carries two extra fields.** Its parameters add an `http` object and a `faults` array, and its result adds `http`; `docs/stall-protocol.md` fixes the parameters as `protocol`, `stall` and `state` and the result as `ready`, `version` and `jobs`. They are spike-only: the spike stall binds its own loopback listener instead of receiving barn's descriptors, and `faults` lets one monitor's upstream throw or loop so fault isolation can be measured. None of it settles B1's `stall.start` fields.

## The workload

- Monitors: 60% `github-issues` (`repo-<i>`, label `atelier:build`), 40% `jira-query` (`project = PROJ<i> ORDER BY key ASC`), each monitor with its own upstream identity so latency can be attributed per monitor.
- Poll interval: 15 s, the shortest sourcefed permits (its default is 60 s). Every figure is therefore also reported per poll and per second, and a run at the real default is four times lighter than what is measured here.
- Upstream: a separate mock-upstream process that synthesises payloads shaped and sized like sourcefed's recorded fixtures — 100 issues per response with the real field set, one extra item carrying `pull_request` so the filter is exercised — and slides a window by one item per request, so each poll after priming sees exactly one new item and one that disappeared. In the quiet scenario the ETag is stable and polls answer 304.
- Client: a fake host subscribes to `GET /events` and acknowledges in batches every 2 s, so queues stay bounded as a real host keeps them.
- Priming: no window starts until every monitor has made one request, capped at 45 s. All three poll on their first 5 s tick, so priming is set by that tick rather than by the runtime; a fault run's chaos monitor is exempt, because all three inject the fault before any broker call, so it never makes one. Every poll counted in a window is therefore a post-priming poll, and the candidates are compared on the same cadence.
- Floor: a candidate that kept up polls each monitor floor(window ÷ interval) − 1 to floor(window ÷ interval) times inside the window. A run below that floor is marked `behind`, and the column carries 1 when it happened.
- Window: 45 s for the steady runs, 30 s for the quiet and fault runs, 20 s for idle, and 30 s per size in the ramp, each after a 2 s warm-up.
- Runs: 10 per candidate per size for the steady and idle scenarios, 5 for quiet, the storage-isolated runs and the fault runs, and 1 per size in the ramp, interleaved A, B (seq), B (realm), C, A, B (seq), B (realm), C ... so slow drift on a shared runner hits all four alike.
- Machine: GitHub Actions `ubuntu-latest` (4 vCPU, 16 GiB), one job per scenario and per ramp size with that group's four variants interleaved in it, so every A-against-B-against-C comparison is same-machine; sizes are not compared across machines.

## What is measured, and how

| Figure            | Definition                                                                                                                                                                                                                                                                                                                                        |
| ----------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Startup           | spawn to the first `net.request` reaching the broker (cold, includes loading, listener bind and the first poll)                                                                                                                                                                                                                                   |
| Ready to serve    | spawn to the `stall.start` reply: the stall has loaded, bound its listener and can take a request                                                                                                                                                                                                                                                 |
| Priming           | the time for every monitor to make its first request, capped at 45 s; it is excluded from every window                                                                                                                                                                                                                                            |
| Idle RSS          | median resident memory of the whole process tree over a 20 s window with no monitors                                                                                                                                                                                                                                                              |
| Load RSS          | median, p95 and max resident memory of the process tree, sampled at 1 Hz across the window                                                                                                                                                                                                                                                        |
| CPU per poll      | kernel-reported `utime+stime` of the process tree across the window, divided by completed polls; also as a percentage of one core                                                                                                                                                                                                                 |
| Poll ratio        | completed polls divided by the polls the workload was due (below 1 means the candidate fell behind)                                                                                                                                                                                                                                               |
| Event latency     | for each delivered event: the time from the broker receiving the upstream's answer for that monitor to the harness parsing the SSE frame carrying it. Both stamps come from the harness's own clock, so no cross-process clock skew is involved, and the figure is the stall's post-upstream work: parse, diff, dedupe, persist, queue, SSE write |
| End-to-end        | the same, from the moment the request was written to the stall protocol                                                                                                                                                                                                                                                                           |
| Throughput limit  | the size at which polls fall behind their interval, found by a ramp over 100, 250, 500 and 1000 monitors                                                                                                                                                                                                                                          |
| Fault containment | with one monitor's upstream forced to throw, and then to loop synchronously: whether `stall.health` keeps answering, whether the other monitors keep delivering, and how long until delivery resumes                                                                                                                                              |
| Realm build       | the median of each run's slowest plugin build, for candidates B and C only; candidate A logs no realm                                                                                                                                                                                                                                             |
| Realm resets      | the median count of realms torn down over the window, for candidates B and C only                                                                                                                                                                                                                                                                 |
| Package size      | candidate A: the vendored build's `dist` bytes plus the published package's packed size plus the Node 26 binary it needs; candidates B and C: the stripped release binaries                                                                                                                                                                       |
| Port cost         | non-blank source lines of the TypeScript that a port has to reproduce, per package, counted from the pinned tree                                                                                                                                                                                                                                  |

Every value in the tables is a median over the runs a group holds, with the range across them in brackets; a metric with no sample is left out of its cell, and a cell is — when no run had a sample.

## Results

Each table below is the harness's own aggregate output for one group of runs, copied as measured, one table per variant of the group so every row sits on the same machine as its rivals. `n` is how many runs the row holds; `widest spread` names the column whose run-to-run range was widest and gives that range as a percentage of its median. `behind` is 1 for a run below the priming floor, and a group's cell is the median of those 0/1 values, so 0 [0–1] means some runs fell below it and some did not. `realm build ms` and `realm resets` exist only for candidates B and C, so candidate A's cells are —. CPU % core can exceed 100% when the process tree uses several cores. The groups ran on different machines: a Xeon Platinum 8370C at 10 monitors, an EPYC 7763 at 100 monitors, the storage-isolated and quiet runs and idle, a Xeon 6973P-C at 500, and an EPYC 9V74 for the faults and the ramp.

### Steady state, 10 monitors

Every variant keeps every poll. Footprint and CPU separate them: candidate A holds 79.3 MB and spends 11.00 ms of CPU a poll, candidate B 11.4 MB with one realm per type at 5.00 ms and 9.1 MB sequential at 4.50, and candidate C 15.4 MB at 5.00. A realm costs 4.6–5.0 ms to build under QuickJS and 5.6 ms under djs, and none of the four reset one.

Candidate A, chaos none, store json:

|   n |             startup ms | idle RSS MB |    RSS median MB |         CPU ms/poll |    CPU % core |       poll ratio |    polls/monitor |  behind |         events/s | p95 latency ms | p99 latency ms | health failures |      drain ms | realm build ms | realm resets | widest spread        |
| --: | ---------------------: | ----------: | ---------------: | ------------------: | ------------: | ---------------: | ---------------: | ------: | ---------------: | -------------: | -------------: | --------------: | ------------: | -------------: | -----------: | -------------------- |
|  10 | 5094.4 [5091.2–5100.0] |           — | 79.3 [79.0–80.2] | 11.00 [11.00–12.00] | 0.5 [0.5–0.5] | 0.67 [0.67–0.67] | 2.00 [2.00–2.00] | 0 [0–0] | 0.89 [0.89–0.89] |  3.9 [3.3–4.5] |  7.6 [4.2–9.8] |         0 [0–0] | 6.7 [5.7–7.5] |              — |            — | p99 latency ms 73.1% |

Candidate B, scheduler seq, chaos none, store json:

|   n |             startup ms | idle RSS MB | RSS median MB |      CPU ms/poll |    CPU % core |       poll ratio |    polls/monitor |  behind |         events/s | p95 latency ms | p99 latency ms | health failures |      drain ms | realm build ms | realm resets | widest spread        |
| --: | ---------------------: | ----------: | ------------: | ---------------: | ------------: | ---------------: | ---------------: | ------: | ---------------: | -------------: | -------------: | --------------: | ------------: | -------------: | -----------: | -------------------- |
|  10 | 5010.2 [5009.4–5010.8] |           — | 9.1 [9.0–9.4] | 4.50 [4.00–5.00] | 0.2 [0.2–0.2] | 0.67 [0.67–0.67] | 2.00 [2.00–2.00] | 0 [0–0] | 0.89 [0.89–0.89] |  3.0 [2.6–4.1] |  3.4 [3.0–4.7] |         0 [0–0] | 2.1 [1.7–2.7] |  4.6 [3.7–4.9] |      0 [0–0] | p99 latency ms 50.2% |

Candidate B, scheduler realm, chaos none, store json:

|   n |             startup ms | idle RSS MB |    RSS median MB |      CPU ms/poll |    CPU % core |       poll ratio |    polls/monitor |  behind |         events/s | p95 latency ms | p99 latency ms | health failures |      drain ms | realm build ms | realm resets | widest spread        |
| --: | ---------------------: | ----------: | ---------------: | ---------------: | ------------: | ---------------: | ---------------: | ------: | ---------------: | -------------: | -------------: | --------------: | ------------: | -------------: | -----------: | -------------------- |
|  10 | 5010.0 [5008.7–5011.5] |           — | 11.4 [11.1–11.6] | 5.00 [4.00–5.00] | 0.2 [0.2–0.2] | 0.67 [0.67–0.67] | 2.00 [2.00–2.00] | 0 [0–0] | 0.89 [0.89–0.89] |  5.0 [4.3–7.7] |  6.3 [4.4–8.0] |         0 [0–0] | 2.2 [2.1–2.9] |  5.0 [4.2–6.3] |      0 [0–0] | p95 latency ms 67.7% |

Candidate C, scheduler realm, engine djs, chaos none, store json:

|   n |             startup ms | idle RSS MB |    RSS median MB |      CPU ms/poll |    CPU % core |       poll ratio |    polls/monitor |  behind |         events/s | p95 latency ms | p99 latency ms | health failures |      drain ms | realm build ms | realm resets | widest spread        |
| --: | ---------------------: | ----------: | ---------------: | ---------------: | ------------: | ---------------: | ---------------: | ------: | ---------------: | -------------: | -------------: | --------------: | ------------: | -------------: | -----------: | -------------------- |
|  10 | 5010.5 [5009.6–5013.1] |           — | 15.4 [15.3–15.6] | 5.00 [5.00–5.50] | 0.2 [0.2–0.2] | 0.67 [0.67–0.67] | 2.00 [2.00–2.00] | 0 [0–0] | 0.89 [0.89–0.89] |  5.5 [5.0–9.3] | 6.1 [5.1–10.9] |         0 [0–0] | 2.4 [2.2–3.0] |  5.6 [4.6–8.0] |      0 [0–0] | p99 latency ms 95.6% |

### Steady state, 100 monitors

Every variant keeps every poll. Candidate A's footprint reaches 177.9 MB and its CPU per poll 41.20 ms, against 18.1 MB and 12.25 ms for B with one realm per type, 15.7 MB and 12.25 sequential, and 21.5 MB and 12.75 for C — 3.4 MB and half a millisecond more than B. A's p95 of 10.6 ms is the lowest of the four, against 420.9, 366.1 and 409.9: at this size the serialised whole-file store, not the runtime, is what delivery waits behind.

Candidate A, chaos none, store json:

|   n |             startup ms | idle RSS MB |       RSS median MB |         CPU ms/poll |       CPU % core |       poll ratio |    polls/monitor |  behind |         events/s |   p95 latency ms |   p99 latency ms | health failures |         drain ms | realm build ms | realm resets | widest spread        |
| --: | ---------------------: | ----------: | ------------------: | ------------------: | ---------------: | ---------------: | ---------------: | ------: | ---------------: | ---------------: | ---------------: | --------------: | ---------------: | -------------: | -----------: | -------------------- |
|  10 | 5105.2 [5103.6–5112.4] |           — | 177.9 [175.0–181.1] | 41.20 [39.15–45.15] | 18.3 [17.4–20.1] | 0.67 [0.67–0.67] | 2.00 [2.00–2.00] | 0 [0–0] | 8.89 [8.89–8.89] | 10.6 [10.2–12.8] | 14.8 [13.0–16.6] |         0 [0–0] | 17.9 [16.9–20.6] |              — |            — | p95 latency ms 25.0% |

Candidate B, scheduler seq, chaos none, store json:

|   n |             startup ms | idle RSS MB |    RSS median MB |         CPU ms/poll |    CPU % core |       poll ratio |    polls/monitor |  behind |         events/s |      p95 latency ms |      p99 latency ms | health failures |      drain ms | realm build ms | realm resets | widest spread  |
| --: | ---------------------: | ----------: | ---------------: | ------------------: | ------------: | ---------------: | ---------------: | ------: | ---------------: | ------------------: | ------------------: | --------------: | ------------: | -------------: | -----------: | -------------- |
|  10 | 5023.2 [5022.8–5025.8] |           — | 15.7 [15.3–15.8] | 12.25 [12.00–12.55] | 5.4 [5.3–5.6] | 0.67 [0.67–0.67] | 2.00 [2.00–2.00] | 0 [0–0] | 8.89 [8.89–8.89] | 366.1 [357.8–402.8] | 405.3 [392.5–418.8] |         0 [0–0] | 3.2 [3.0–3.9] |  4.0 [3.9–4.2] |      0 [0–0] | drain ms 26.8% |

Candidate B, scheduler realm, chaos none, store json:

|   n |             startup ms | idle RSS MB |    RSS median MB |         CPU ms/poll |    CPU % core |       poll ratio |    polls/monitor |  behind |         events/s |      p95 latency ms |      p99 latency ms | health failures |      drain ms | realm build ms | realm resets | widest spread        |
| --: | ---------------------: | ----------: | ---------------: | ------------------: | ------------: | ---------------: | ---------------: | ------: | ---------------: | ------------------: | ------------------: | --------------: | ------------: | -------------: | -----------: | -------------------- |
|  10 | 5023.0 [5022.4–5023.7] |           — | 18.1 [17.8–18.2] | 12.25 [12.10–12.80] | 5.4 [5.4–5.7] | 0.67 [0.67–0.67] | 2.00 [2.00–2.00] | 0 [0–0] | 8.89 [8.89–8.89] | 420.9 [415.9–447.8] | 449.1 [431.0–471.2] |         0 [0–0] | 3.3 [3.1–4.0] |  4.3 [4.1–5.6] |      0 [0–0] | realm build ms 34.9% |

Candidate C, scheduler realm, engine djs, chaos none, store json:

|   n |             startup ms | idle RSS MB |    RSS median MB |         CPU ms/poll |    CPU % core |       poll ratio |    polls/monitor |  behind |         events/s |      p95 latency ms |      p99 latency ms | health failures |      drain ms | realm build ms | realm resets | widest spread        |
| --: | ---------------------: | ----------: | ---------------: | ------------------: | ------------: | ---------------: | ---------------: | ------: | ---------------: | ------------------: | ------------------: | --------------: | ------------: | -------------: | -----------: | -------------------- |
|  10 | 5023.5 [5022.9–5024.7] |           — | 21.5 [19.8–21.7] | 12.75 [12.55–13.05] | 5.7 [5.6–5.8] | 0.67 [0.67–0.67] | 2.00 [2.00–2.00] | 0 [0–0] | 8.89 [8.89–8.89] | 409.9 [405.1–429.8] | 435.5 [417.8–457.4] |         0 [0–0] | 4.0 [3.6–4.5] |  5.0 [4.9–6.1] |      0 [0–0] | realm build ms 24.0% |

### Steady state, 500 monitors

Here the runtime and the store stop being separable. Candidate A falls behind (1.01 polls a monitor, `behind` 1, 29.87 events/s) at 112.46 ms of CPU a poll and 320.6 MB, while all three Rust variants keep every poll (2.00 polls a monitor) and deliver 29.69, 30.93 and 29.69 events a second at 34.07 ms on C, 34.01 with one realm per type and 33.84 sequential, in 48.4, 47.3 and 41.1 MB. Delivery now queues behind the store, so the p95 is 11.9 s on C, 14.5 s with one realm per type and 11.5 s sequential, against A's 1.68 s; that figure is a statement about the store, and the memory-store tables below are the reading of the runtime without it.

Candidate A, chaos none, store json:

|   n |             startup ms | idle RSS MB |       RSS median MB |            CPU ms/poll |          CPU % core |       poll ratio |    polls/monitor |  behind |            events/s |       p95 latency ms |         p99 latency ms | health failures |               drain ms | realm build ms | realm resets | widest spread        |
| --: | ---------------------: | ----------: | ------------------: | ---------------------: | ------------------: | ---------------: | ---------------: | ------: | ------------------: | -------------------: | ---------------------: | --------------: | ---------------------: | -------------: | -----------: | -------------------- |
|  10 | 5084.6 [5080.0–5088.4] |           — | 320.6 [311.6–334.0] | 112.46 [108.47–118.02] | 126.2 [125.8–127.5] | 0.34 [0.32–0.35] | 1.01 [0.97–1.04] | 1 [1–1] | 29.87 [22.82–33.20] | 1684.6 [98.8–1757.5] | 2005.4 [1604.1–2037.2] |         0 [0–0] | 3025.8 [3017.4–3037.2] |              — |            — | p95 latency ms 98.5% |

Candidate B, scheduler seq, chaos none, store json:

|   n |             startup ms | idle RSS MB |    RSS median MB |         CPU ms/poll |       CPU % core |       poll ratio |    polls/monitor |  behind |            events/s |            p95 latency ms |            p99 latency ms | health failures |      drain ms | realm build ms | realm resets | widest spread  |
| --: | ---------------------: | ----------: | ---------------: | ------------------: | ---------------: | ---------------: | ---------------: | ------: | ------------------: | ------------------------: | ------------------------: | --------------: | ------------: | -------------: | -----------: | -------------- |
|  10 | 5221.2 [5217.8–5249.9] |           — | 41.1 [36.5–46.0] | 33.84 [33.04–34.69] | 75.2 [73.4–77.1] | 0.67 [0.67–0.67] | 2.00 [2.00–2.00] | 0 [0–0] | 30.93 [29.11–31.78] | 11496.8 [11084.6–14589.2] | 14320.5 [11457.0–15053.2] |         0 [0–0] | 4.8 [4.0–8.4] |  2.9 [2.8–3.3] |      0 [0–0] | drain ms 91.4% |

Candidate B, scheduler realm, chaos none, store json:

|   n |             startup ms | idle RSS MB |    RSS median MB |         CPU ms/poll |       CPU % core |       poll ratio |    polls/monitor |  behind |            events/s |            p95 latency ms |            p99 latency ms | health failures |       drain ms | realm build ms | realm resets | widest spread   |
| --: | ---------------------: | ----------: | ---------------: | ------------------: | ---------------: | ---------------: | ---------------: | ------: | ------------------: | ------------------------: | ------------------------: | --------------: | -------------: | -------------: | -----------: | --------------- |
|  10 | 5224.9 [5220.4–5239.0] |           — | 47.3 [34.4–51.3] | 34.01 [33.52–35.22] | 75.6 [74.5–78.3] | 0.67 [0.67–0.67] | 2.00 [2.00–2.00] | 0 [0–0] | 29.69 [28.76–31.38] | 14451.0 [11564.1–14974.3] | 14807.3 [11953.6–15326.6] |         0 [0–0] | 4.8 [4.1–12.2] |  3.3 [2.9–4.0] |      0 [0–0] | drain ms 167.1% |

Candidate C, scheduler realm, engine djs, chaos none, store json:

|   n |             startup ms | idle RSS MB |    RSS median MB |         CPU ms/poll |       CPU % core |       poll ratio |    polls/monitor |  behind |            events/s |            p95 latency ms |            p99 latency ms | health failures |      drain ms | realm build ms | realm resets | widest spread        |
| --: | ---------------------: | ----------: | ---------------: | ------------------: | ---------------: | ---------------: | ---------------: | ------: | ------------------: | ------------------------: | ------------------------: | --------------: | ------------: | -------------: | -----------: | -------------------- |
|  10 | 5229.2 [5218.9–5250.2] |           — | 48.4 [47.8–50.6] | 34.07 [33.51–34.83] | 75.7 [74.5–77.4] | 0.67 [0.67–0.67] | 2.00 [2.00–2.00] | 0 [0–0] | 29.69 [28.93–30.80] | 11906.9 [11229.8–14274.6] | 14525.2 [11605.5–14928.5] |         0 [0–0] | 5.2 [4.8–5.6] |  4.4 [3.8–5.1] |      0 [0–0] | realm build ms 29.5% |

### Quiet upstream at 100 monitors

A stable ETag changes none of the ordering. Every variant keeps its poll and delivers nothing; candidate A costs 40.10 ms of CPU a poll against 12.30 on C, 12.10 with one realm per type and 11.90 sequential, and holds 166.9 MB against 19.7, 16.3 and 14.1. A 304 is no cheaper than a changed poll for any of them: the mock upstream's body is small either way, and the work is the stall's own.

Candidate A, chaos none, store json:

|   n |             startup ms | idle RSS MB |       RSS median MB |         CPU ms/poll |       CPU % core |       poll ratio |    polls/monitor |  behind |         events/s | p95 latency ms | p99 latency ms | health failures |         drain ms | realm build ms | realm resets | widest spread      |
| --: | ---------------------: | ----------: | ------------------: | ------------------: | ---------------: | ---------------: | ---------------: | ------: | ---------------: | -------------: | -------------: | --------------: | ---------------: | -------------: | -----------: | ------------------ |
|   5 | 5104.6 [5102.7–5113.2] |           — | 166.9 [161.4–174.3] | 40.10 [38.40–41.10] | 13.4 [12.8–13.7] | 0.50 [0.50–0.50] | 1.00 [1.00–1.00] | 0 [0–0] | 0.00 [0.00–0.00] |              — |              — |         0 [0–0] | 17.7 [17.3–18.4] |              — |            — | RSS median MB 7.8% |

Candidate B, scheduler seq, chaos none, store json:

|   n |             startup ms | idle RSS MB |    RSS median MB |         CPU ms/poll |    CPU % core |       poll ratio |    polls/monitor |  behind |         events/s | p95 latency ms | p99 latency ms | health failures |      drain ms | realm build ms | realm resets | widest spread  |
| --: | ---------------------: | ----------: | ---------------: | ------------------: | ------------: | ---------------: | ---------------: | ------: | ---------------: | -------------: | -------------: | --------------: | ------------: | -------------: | -----------: | -------------- |
|   5 | 5023.9 [5023.4–5025.1] |           — | 14.1 [12.6–14.3] | 11.90 [11.90–12.10] | 4.0 [4.0–4.0] | 0.50 [0.50–0.50] | 1.00 [1.00–1.00] | 0 [0–0] | 0.00 [0.00–0.00] |              — |              — |         0 [0–0] | 2.9 [2.4–3.1] |  3.9 [3.9–4.7] |      0 [0–0] | drain ms 25.3% |

Candidate B, scheduler realm, chaos none, store json:

|   n |             startup ms | idle RSS MB |    RSS median MB |         CPU ms/poll |    CPU % core |       poll ratio |    polls/monitor |  behind |         events/s | p95 latency ms | p99 latency ms | health failures |      drain ms | realm build ms | realm resets | widest spread        |
| --: | ---------------------: | ----------: | ---------------: | ------------------: | ------------: | ---------------: | ---------------: | ------: | ---------------: | -------------: | -------------: | --------------: | ------------: | -------------: | -----------: | -------------------- |
|   5 | 5023.9 [5023.5–5024.8] |           — | 16.3 [16.1–18.8] | 12.10 [11.90–12.20] | 4.0 [4.0–4.1] | 0.50 [0.50–0.50] | 1.00 [1.00–1.00] | 0 [0–0] | 0.00 [0.00–0.00] |              — |              — |         0 [0–0] | 3.1 [3.1–3.5] |  4.9 [4.0–5.2] |      0 [0–0] | realm build ms 24.5% |

Candidate C, scheduler realm, engine djs, chaos none, store json:

|   n |             startup ms | idle RSS MB |    RSS median MB |         CPU ms/poll |    CPU % core |       poll ratio |    polls/monitor |  behind |         events/s | p95 latency ms | p99 latency ms | health failures |      drain ms | realm build ms | realm resets | widest spread  |
| --: | ---------------------: | ----------: | ---------------: | ------------------: | ------------: | ---------------: | ---------------: | ------: | ---------------: | -------------: | -------------: | --------------: | ------------: | -------------: | -----------: | -------------- |
|   5 | 5023.9 [5023.4–5025.4] |           — | 19.7 [18.8–20.6] | 12.30 [12.20–12.70] | 4.1 [4.1–4.2] | 0.50 [0.50–0.50] | 1.00 [1.00–1.00] | 0 [0–0] | 0.00 [0.00–0.00] |              — |              — |         0 [0–0] | 3.6 [3.1–6.1] |  5.4 [4.9–6.1] |      0 [0–0] | drain ms 84.9% |

### Storage isolated at 100 monitors

With the store out of the way candidate A costs 41.50 ms of CPU a poll against 2.30 on C, 2.00 with one realm per type and 1.90 sequential — eighteen to twenty-two times — and holds 151.0 MB against 16.6, 12.5 and 10.1. Here, while all four keep up, A's delivered-event p95 is the lowest: 24.3 ms against 38.4, 37.3 and 36.8.

Candidate A, chaos none, store memory:

|   n |             startup ms | idle RSS MB |       RSS median MB |         CPU ms/poll |       CPU % core |       poll ratio |    polls/monitor |  behind |         events/s |   p95 latency ms |   p99 latency ms | health failures |         drain ms | realm build ms | realm resets | widest spread |
| --: | ---------------------: | ----------: | ------------------: | ------------------: | ---------------: | ---------------: | ---------------: | ------: | ---------------: | ---------------: | ---------------: | --------------: | ---------------: | -------------: | -----------: | ------------- |
|   5 | 5104.3 [5098.3–5113.6] |           — | 151.0 [150.8–151.1] | 41.50 [41.20–43.10] | 13.8 [13.7–14.4] | 0.50 [0.50–0.50] | 1.00 [1.00–1.00] | 0 [0–0] | 6.67 [6.67–6.67] | 24.3 [24.0–26.0] | 25.7 [25.3–27.1] |         0 [0–0] | 12.5 [12.0–13.1] |              — |            — | drain ms 8.7% |

Candidate B, scheduler seq, chaos none, store memory:

|   n |             startup ms | idle RSS MB |    RSS median MB |      CPU ms/poll |    CPU % core |       poll ratio |    polls/monitor |  behind |         events/s |   p95 latency ms |   p99 latency ms | health failures |      drain ms | realm build ms | realm resets | widest spread  |
| --: | ---------------------: | ----------: | ---------------: | ---------------: | ------------: | ---------------: | ---------------: | ------: | ---------------: | ---------------: | ---------------: | --------------: | ------------: | -------------: | -----------: | -------------- |
|   5 | 5010.8 [5010.0–5011.2] |           — | 10.1 [10.1–10.2] | 1.90 [1.70–1.90] | 0.6 [0.6–0.6] | 0.50 [0.50–0.50] | 1.00 [1.00–1.00] | 0 [0–0] | 6.67 [6.67–6.67] | 36.8 [36.4–37.0] | 40.1 [39.8–40.7] |         0 [0–0] | 2.7 [2.4–2.8] |  4.0 [4.0–4.3] |      0 [0–0] | drain ms 16.4% |

Candidate B, scheduler realm, chaos none, store memory:

|   n |             startup ms | idle RSS MB |    RSS median MB |      CPU ms/poll |    CPU % core |       poll ratio |    polls/monitor |  behind |         events/s |   p95 latency ms |   p99 latency ms | health failures |      drain ms | realm build ms | realm resets | widest spread  |
| --: | ---------------------: | ----------: | ---------------: | ---------------: | ------------: | ---------------: | ---------------: | ------: | ---------------: | ---------------: | ---------------: | --------------: | ------------: | -------------: | -----------: | -------------- |
|   5 | 5010.0 [5009.6–5010.5] |           — | 12.5 [12.4–12.6] | 2.00 [1.90–2.10] | 0.7 [0.6–0.7] | 0.50 [0.50–0.50] | 1.00 [1.00–1.00] | 0 [0–0] | 6.67 [6.67–6.67] | 37.3 [31.7–37.8] | 40.5 [38.8–40.7] |         0 [0–0] | 3.0 [2.6–3.4] |  5.2 [4.4–5.4] |      0 [0–0] | drain ms 27.0% |

Candidate C, scheduler realm, engine djs, chaos none, store memory:

|   n |             startup ms | idle RSS MB |    RSS median MB |      CPU ms/poll |    CPU % core |       poll ratio |    polls/monitor |  behind |         events/s |  p95 latency ms |  p99 latency ms | health failures |      drain ms | realm build ms | realm resets | widest spread        |
| --: | ---------------------: | ----------: | ---------------: | ---------------: | ------------: | ---------------: | ---------------: | ------: | ---------------: | --------------: | --------------: | --------------: | ------------: | -------------: | -----------: | -------------------- |
|   5 | 5011.1 [5010.1–5011.7] |           — | 16.6 [16.5–18.2] | 2.30 [2.30–2.50] | 0.8 [0.8–0.8] | 0.50 [0.50–0.50] | 1.00 [1.00–1.00] | 0 [0–0] | 6.67 [6.67–6.67] | 38.4 [3.6–39.7] | 41.8 [6.5–42.3] |         0 [0–0] | 3.2 [3.2–3.7] |  5.6 [4.9–6.3] |      0 [0–0] | p95 latency ms 94.0% |

### Storage isolated at 500 monitors

At 500 monitors with the store gone the gap is 144.66 against 3.04, 2.74 and 2.56 ms a poll — forty-eight to fifty-seven times — and it is a difference in capacity rather than in cost: candidate A needs 107.5% of one core and still reaches only 0.45 polls a monitor with a 2.10 s p95, while B needs 4.6% of a core and C 5.1%, keep every poll and deliver at 76.1 ms on C, 102.9 with one realm per type and 41.0 sequential.

Candidate A, chaos none, store memory:

|   n |             startup ms | idle RSS MB |       RSS median MB |            CPU ms/poll |          CPU % core |       poll ratio |    polls/monitor |  behind |           events/s |         p95 latency ms |         p99 latency ms | health failures |               drain ms | realm build ms | realm resets | widest spread        |
| --: | ---------------------: | ----------: | ------------------: | ---------------------: | ------------------: | ---------------: | ---------------: | ------: | -----------------: | ---------------------: | ---------------------: | --------------: | ---------------------: | -------------: | -----------: | -------------------- |
|   5 | 5105.9 [5103.1–5113.6] |           — | 202.0 [198.1–208.6] | 144.66 [141.54–147.61] | 107.5 [107.3–107.6] | 0.22 [0.22–0.23] | 0.45 [0.44–0.46] | 1 [1–1] | 10.07 [9.73–10.33] | 2098.8 [1905.3–2164.4] | 2254.4 [1955.7–2304.3] |         0 [0–1] | 4921.7 [4394.8–5097.3] |              — |            — | p99 latency ms 15.5% |

Candidate B, scheduler seq, chaos none, store memory:

|   n |             startup ms | idle RSS MB |    RSS median MB |      CPU ms/poll |    CPU % core |       poll ratio |    polls/monitor |  behind |            events/s |   p95 latency ms |   p99 latency ms | health failures |      drain ms | realm build ms | realm resets | widest spread        |
| --: | ---------------------: | ----------: | ---------------: | ---------------: | ------------: | ---------------: | ---------------: | ------: | ------------------: | ---------------: | ---------------: | --------------: | ------------: | -------------: | -----------: | -------------------- |
|   5 | 5011.8 [5011.0–5012.0] |           — | 16.4 [16.4–16.5] | 2.56 [2.50–2.68] | 4.3 [4.2–4.5] | 0.50 [0.50–0.50] | 1.00 [1.00–1.00] | 0 [0–0] | 33.33 [33.33–33.33] | 41.0 [38.0–49.8] | 50.7 [42.5–62.6] |         0 [0–0] | 3.7 [3.4–4.2] |  4.0 [4.0–4.2] |      0 [0–0] | p99 latency ms 39.9% |

Candidate B, scheduler realm, chaos none, store memory:

|   n |             startup ms | idle RSS MB |    RSS median MB |      CPU ms/poll |    CPU % core |       poll ratio |    polls/monitor |  behind |            events/s |     p95 latency ms |     p99 latency ms | health failures |      drain ms | realm build ms | realm resets | widest spread   |
| --: | ---------------------: | ----------: | ---------------: | ---------------: | ------------: | ---------------: | ---------------: | ------: | ------------------: | -----------------: | -----------------: | --------------: | ------------: | -------------: | -----------: | --------------- |
|   5 | 5010.9 [5010.6–5011.4] |           — | 19.1 [19.0–23.1] | 2.74 [2.68–2.78] | 4.6 [4.5–4.6] | 0.50 [0.50–0.50] | 1.00 [1.00–1.00] | 0 [0–0] | 33.33 [33.33–33.33] | 102.9 [84.9–112.2] | 112.8 [98.1–119.8] |         0 [0–0] | 4.8 [4.2–9.8] |  4.3 [4.2–4.4] |      0 [0–0] | drain ms 117.2% |

Candidate C, scheduler realm, engine djs, chaos none, store memory:

|   n |             startup ms | idle RSS MB |    RSS median MB |      CPU ms/poll |    CPU % core |       poll ratio |    polls/monitor |  behind |            events/s |    p95 latency ms |    p99 latency ms | health failures |      drain ms | realm build ms | realm resets | widest spread        |
| --: | ---------------------: | ----------: | ---------------: | ---------------: | ------------: | ---------------: | ---------------: | ------: | ------------------: | ----------------: | ----------------: | --------------: | ------------: | -------------: | -----------: | -------------------- |
|   5 | 5012.4 [5011.5–5012.9] |           — | 23.0 [22.8–27.0] | 3.04 [3.00–3.12] | 5.1 [5.0–5.2] | 0.50 [0.50–0.50] | 1.00 [1.00–1.00] | 0 [0–0] | 33.33 [33.33–33.33] | 76.1 [64.6–115.2] | 89.0 [77.8–125.8] |         0 [0–0] | 4.9 [4.3–6.4] |  6.2 [5.1–6.4] |      0 [0–0] | p95 latency ms 66.5% |

### Idle

With no monitors at all candidate A holds 70.4 MB against 7.9 MB for B with one realm per type, 6.0 MB sequential and 7.9 MB for C, so the floor of a Node process is nine to twelve times the floor of either binary, before any monitor exists; C's footprint with no realm built is identical to B's.

Candidate A, chaos none, store json:

|   n | startup ms |      idle RSS MB |    RSS median MB |        CPU ms/poll |    CPU % core | poll ratio | polls/monitor | behind |         events/s | p95 latency ms | p99 latency ms | health failures |      drain ms | realm build ms | realm resets | widest spread      |
| --: | ---------: | ---------------: | ---------------: | -----------------: | ------------: | ---------: | ------------: | -----: | ---------------: | -------------: | -------------: | --------------: | ------------: | -------------: | -----------: | ------------------ |
|  10 |          — | 70.4 [70.3–70.5] | 70.4 [70.3–70.5] | 10.00 [0.00–20.00] | 0.1 [0.0–0.1] |          — |             — |      — | 0.00 [0.00–0.00] |              — |              — |         0 [0–0] | 8.0 [7.4–8.6] |              — |            — | CPU ms/poll 200.0% |

Candidate B, scheduler seq, chaos none, store json:

|   n | startup ms |   idle RSS MB | RSS median MB |      CPU ms/poll |    CPU % core | poll ratio | polls/monitor | behind |         events/s | p95 latency ms | p99 latency ms | health failures |      drain ms | realm build ms | realm resets | widest spread  |
| --: | ---------: | ------------: | ------------: | ---------------: | ------------: | ---------: | ------------: | -----: | ---------------: | -------------: | -------------: | --------------: | ------------: | -------------: | -----------: | -------------- |
|  10 |          — | 6.0 [5.9–6.1] | 6.0 [5.9–6.1] | 0.00 [0.00–0.00] | 0.0 [0.0–0.0] |          — |             — |      — | 0.00 [0.00–0.00] |              — |              — |         0 [0–0] | 2.1 [1.8–2.5] |              — |            — | drain ms 31.2% |

Candidate B, scheduler realm, chaos none, store json:

|   n | startup ms |   idle RSS MB | RSS median MB |      CPU ms/poll |    CPU % core | poll ratio | polls/monitor | behind |         events/s | p95 latency ms | p99 latency ms | health failures |      drain ms | realm build ms | realm resets | widest spread  |
| --: | ---------: | ------------: | ------------: | ---------------: | ------------: | ---------: | ------------: | -----: | ---------------: | -------------: | -------------: | --------------: | ------------: | -------------: | -----------: | -------------- |
|  10 |          — | 7.9 [7.8–8.0] | 7.9 [7.8–8.0] | 0.00 [0.00–0.00] | 0.0 [0.0–0.0] |          — |             — |      — | 0.00 [0.00–0.00] |              — |              — |         0 [0–0] | 2.2 [1.8–2.6] |              — |            — | drain ms 34.9% |

Candidate C, scheduler realm, engine djs, chaos none, store json:

|   n | startup ms |   idle RSS MB | RSS median MB |      CPU ms/poll |    CPU % core | poll ratio | polls/monitor | behind |         events/s | p95 latency ms | p99 latency ms | health failures |      drain ms | realm build ms | realm resets | widest spread  |
| --: | ---------: | ------------: | ------------: | ---------------: | ------------: | ---------: | ------------: | -----: | ---------------: | -------------: | -------------: | --------------: | ------------: | -------------: | -----------: | -------------- |
|  10 |          — | 7.9 [7.8–7.9] | 7.9 [7.8–7.9] | 0.00 [0.00–0.00] | 0.0 [0.0–0.0] |          — |             — |      — | 0.00 [0.00–0.00] |              — |              — |         0 [0–0] | 2.2 [1.9–2.6] |              — |            — | drain ms 33.3% |

### One plugin throws

A throwing upstream is handled the same way by all four: 0.95 polls a monitor, 1.33 events a second, no health failure and a clean drain (7.9 ms for A, 2.5 on C, 2.2 and 2.1 for the two QuickJS schedulers). Whatever separates the candidates, it is not error handling.

Candidate A, chaos throw, store json:

|   n |             startup ms | idle RSS MB |    RSS median MB |         CPU ms/poll |    CPU % core |       poll ratio |    polls/monitor |  behind |         events/s | p95 latency ms |   p99 latency ms | health failures |       drain ms | realm build ms | realm resets | widest spread          |
| --: | ---------------------: | ----------: | ---------------: | ------------------: | ------------: | ---------------: | ---------------: | ------: | ---------------: | -------------: | ---------------: | --------------: | -------------: | -------------: | -----------: | ---------------------- |
|   5 | 5082.2 [5081.2–5084.8] |           — | 89.1 [88.9–89.4] | 14.00 [14.00–14.50] | 0.9 [0.9–1.0] | 0.48 [0.48–0.48] | 0.95 [0.95–0.95] | 1 [1–1] | 1.33 [1.33–1.33] |  4.0 [3.5–7.1] | 17.0 [4.3–210.6] |         0 [0–0] | 7.9 [7.2–44.6] |              — |            — | p99 latency ms 1216.0% |

Candidate B, scheduler seq, chaos throw, store json:

|   n |             startup ms | idle RSS MB | RSS median MB |      CPU ms/poll |    CPU % core |       poll ratio |    polls/monitor |  behind |         events/s | p95 latency ms | p99 latency ms | health failures |      drain ms | realm build ms | realm resets | widest spread         |
| --: | ---------------------: | ----------: | ------------: | ---------------: | ------------: | ---------------: | ---------------: | ------: | ---------------: | -------------: | -------------: | --------------: | ------------: | -------------: | -----------: | --------------------- |
|   5 | 5009.8 [5009.4–5009.9] |           — | 9.4 [9.2–9.6] | 4.50 [4.00–5.00] | 0.3 [0.3–0.3] | 0.48 [0.48–0.48] | 0.95 [0.95–0.95] | 1 [1–1] | 1.33 [1.33–1.33] | 5.1 [3.7–35.5] | 5.3 [3.8–35.6] |         0 [0–0] | 2.1 [1.7–2.3] |  3.2 [3.2–3.5] |      0 [0–0] | p95 latency ms 627.2% |

Candidate B, scheduler realm, chaos throw, store json:

|   n |             startup ms | idle RSS MB |    RSS median MB |      CPU ms/poll |    CPU % core |       poll ratio |    polls/monitor |  behind |         events/s |   p95 latency ms |   p99 latency ms | health failures |      drain ms | realm build ms | realm resets | widest spread        |
| --: | ---------------------: | ----------: | ---------------: | ---------------: | ------------: | ---------------: | ---------------: | ------: | ---------------: | ---------------: | ---------------: | --------------: | ------------: | -------------: | -----------: | -------------------- |
|   5 | 5008.5 [5008.3–5008.9] |           — | 11.8 [11.5–12.2] | 4.50 [4.00–5.00] | 0.3 [0.3–0.3] | 0.48 [0.48–0.48] | 0.95 [0.95–0.95] | 1 [1–1] | 1.33 [1.33–1.33] | 14.5 [10.6–20.7] | 14.7 [11.0–20.9] |         0 [0–0] | 2.2 [2.0–2.4] |  3.8 [3.2–4.1] |      0 [0–0] | p95 latency ms 69.1% |

Candidate C, scheduler realm, engine djs, chaos throw, store json:

|   n |             startup ms | idle RSS MB |    RSS median MB |      CPU ms/poll |    CPU % core |       poll ratio |    polls/monitor |  behind |         events/s |   p95 latency ms |   p99 latency ms | health failures |      drain ms | realm build ms | realm resets | widest spread         |
| --: | ---------------------: | ----------: | ---------------: | ---------------: | ------------: | ---------------: | ---------------: | ------: | ---------------: | ---------------: | ---------------: | --------------: | ------------: | -------------: | -----------: | --------------------- |
|   5 | 5009.3 [5008.7–5010.4] |           — | 15.8 [15.6–15.9] | 5.00 [4.50–5.00] | 0.3 [0.3–0.3] | 0.48 [0.48–0.48] | 0.95 [0.95–0.95] | 1 [1–1] | 1.33 [1.33–1.33] | 14.2 [12.6–38.4] | 14.6 [13.0–38.6] |         0 [0–0] | 2.5 [2.3–2.9] |  4.4 [4.0–4.6] |      0 [0–0] | p95 latency ms 180.9% |

### One plugin loops

Here the four stop being comparable. Candidate A delivers nothing for the whole 30 s window, stops answering health probes (three failures, after which the harness marks it unresponsive) and never finishes `stall.drain`, so the harness kills it at its 10 s timeout, five runs with no spread. Candidate B keeps answering every health probe, keeps delivering the other 20 monitors' events (1.33 a second, p95 6.8 ms sequential and 16.1 ms with one realm per type) and drains in 1.9 and 2.0 ms; each runaway poll is stopped at its 10 s deadline and the realm rebuilt, which costs about 904 ms of CPU per completed poll, about 60% of a core over the window. Candidate C is the same shape at 853.00 ms a poll and 56.9% of a core, two resets, a p95 of 14.5 ms and a 2.4 ms drain: its reset is a copy of the snapshot rather than a rebuild, and the `realm-reset` line times it.

Candidate A, chaos loop, store json:

|   n |             startup ms | idle RSS MB |    RSS median MB |               CPU ms/poll |       CPU % core |       poll ratio |    polls/monitor |  behind |         events/s | p95 latency ms | p99 latency ms | health failures |                  drain ms | realm build ms | realm resets | widest spread    |
| --: | ---------------------: | ----------: | ---------------: | ------------------------: | ---------------: | ---------------: | ---------------: | ------: | ---------------: | -------------: | -------------: | --------------: | ------------------------: | -------------: | -----------: | ---------------- |
|   5 | 5081.5 [5081.0–5092.5] |           — | 88.4 [88.1–88.5] | 4010.00 [4010.00–5020.00] | 13.4 [13.4–16.7] | 0.00 [0.00–0.00] | 0.00 [0.00–0.00] | 1 [1–1] | 0.00 [0.00–0.00] |              — |              — |         3 [3–3] | 10011.3 [10011.0–10013.2] |              — |            — | CPU % core 25.2% |

Candidate B, scheduler seq, chaos loop, store json:

|   n |             startup ms | idle RSS MB | RSS median MB |            CPU ms/poll |       CPU % core |       poll ratio |    polls/monitor |  behind |         events/s |  p95 latency ms |  p99 latency ms | health failures |      drain ms | realm build ms | realm resets | widest spread          |
| --: | ---------------------: | ----------: | ------------: | ---------------------: | ---------------: | ---------------: | ---------------: | ------: | ---------------: | --------------: | --------------: | --------------: | ------------: | -------------: | -----------: | ---------------------- |
|   5 | 5009.8 [5009.4–5012.8] |           — | 9.5 [9.4–9.6] | 904.00 [904.00–904.50] | 60.3 [60.3–60.3] | 0.48 [0.48–0.48] | 0.95 [0.95–0.95] | 1 [1–1] | 1.33 [1.33–1.33] | 6.8 [3.8–193.3] | 6.9 [3.9–193.4] |         0 [0–0] | 1.9 [1.7–2.1] |  3.2 [3.2–5.5] |      2 [2–2] | p95 latency ms 2769.2% |

Candidate B, scheduler realm, chaos loop, store json:

|   n |             startup ms | idle RSS MB |    RSS median MB |            CPU ms/poll |       CPU % core |       poll ratio |    polls/monitor |  behind |         events/s |    p95 latency ms |    p99 latency ms | health failures |      drain ms | realm build ms | realm resets | widest spread          |
| --: | ---------------------: | ----------: | ---------------: | ---------------------: | ---------------: | ---------------: | ---------------: | ------: | ---------------: | ----------------: | ----------------: | --------------: | ------------: | -------------: | -----------: | ---------------------- |
|   5 | 5008.4 [5007.7–5009.3] |           — | 11.6 [11.5–11.9] | 903.50 [903.50–905.00] | 60.2 [60.2–60.3] | 0.48 [0.48–0.48] | 0.95 [0.95–0.95] | 1 [1–1] | 1.33 [1.33–1.33] | 16.1 [13.5–196.5] | 16.6 [13.8–196.9] |         0 [0–0] | 2.0 [1.8–2.6] |  3.6 [3.2–4.1] |      2 [2–2] | p95 latency ms 1136.1% |

Candidate C, scheduler realm, engine djs, chaos loop, store json:

|   n |             startup ms | idle RSS MB |    RSS median MB |            CPU ms/poll |       CPU % core |       poll ratio |    polls/monitor |  behind |         events/s |   p95 latency ms |   p99 latency ms | health failures |      drain ms | realm build ms | realm resets | widest spread         |
| --: | ---------------------: | ----------: | ---------------: | ---------------------: | ---------------: | ---------------: | ---------------: | ------: | ---------------: | ---------------: | ---------------: | --------------: | ------------: | -------------: | -----------: | --------------------- |
|   5 | 5009.0 [5008.6–5010.5] |           — | 15.8 [15.8–15.9] | 853.00 [815.00–889.00] | 56.9 [54.3–59.3] | 0.48 [0.48–0.48] | 0.95 [0.95–0.95] | 1 [1–1] | 1.33 [1.33–1.33] | 14.5 [10.0–71.6] | 14.5 [10.7–71.8] |         0 [0–0] | 2.4 [2.2–2.9] |  4.5 [4.1–4.9] |      2 [2–2] | p95 latency ms 425.6% |

### Ramp: 100, 250, 500 and 1000 monitors

Every variant holds the line to 250 monitors, though candidate A's CPU per poll is already 36.30 ms at 100 against 11.00 on C, 11.10 with one realm per type and 10.60 sequential, and 69.48 ms at 250 against 22.80, 23.40 and 23.00; at 250 A's p95 of 74.3 ms is the lowest of the four against 5.0 s on C, 2.2 s with one realm per type and 2.6 s sequential, the store queue again. At 500 candidate A falls behind (0.57 polls a monitor, `behind` 1) while all three Rust variants still poll every monitor once in the 30 s window, C at 41.28 ms a poll against 40.06 and 40.70; at 1000 all four are behind (0.49 on C, 0.50 and 0.51 on B, 0.26 on A). The Rust candidates' delivered-event figure at 1000 is 0.00 and is not comparable: the per-client SSE queue holds 64 frames and dropped the fake host. In the 45 s steady runs the same crossing shows up as A's 1.01 polls a monitor against the Rust variants' 2.00.

100 monitors: candidate A, chaos none, store json:

|   n |             startup ms | idle RSS MB |       RSS median MB |         CPU ms/poll |       CPU % core |       poll ratio |    polls/monitor |  behind |         events/s |   p95 latency ms |   p99 latency ms | health failures |         drain ms | realm build ms | realm resets | widest spread |
| --: | ---------------------: | ----------: | ------------------: | ------------------: | ---------------: | ---------------: | ---------------: | ------: | ---------------: | ---------------: | ---------------: | --------------: | ---------------: | -------------: | -----------: | ------------- |
|   1 | 5085.8 [5085.8–5085.8] |           — | 174.8 [174.8–174.8] | 36.30 [36.30–36.30] | 12.1 [12.1–12.1] | 0.50 [0.50–0.50] | 1.00 [1.00–1.00] | 0 [0–0] | 6.67 [6.67–6.67] | 11.5 [11.5–11.5] | 14.3 [14.3–14.3] |         0 [0–0] | 13.9 [13.9–13.9] |              — |            — | —             |

100 monitors: candidate B, scheduler seq, chaos none, store json:

|   n |             startup ms | idle RSS MB |    RSS median MB |         CPU ms/poll |    CPU % core |       poll ratio |    polls/monitor |  behind |         events/s |      p95 latency ms |      p99 latency ms | health failures |      drain ms | realm build ms | realm resets | widest spread |
| --: | ---------------------: | ----------: | ---------------: | ------------------: | ------------: | ---------------: | ---------------: | ------: | ---------------: | ------------------: | ------------------: | --------------: | ------------: | -------------: | -----------: | ------------- |
|   1 | 5019.4 [5019.4–5019.4] |           — | 12.9 [12.9–12.9] | 10.60 [10.60–10.60] | 3.5 [3.5–3.5] | 0.50 [0.50–0.50] | 1.00 [1.00–1.00] | 0 [0–0] | 6.67 [6.67–6.67] | 795.9 [795.9–795.9] | 801.1 [801.1–801.1] |         0 [0–0] | 2.7 [2.7–2.7] |  3.2 [3.2–3.2] |      0 [0–0] | —             |

100 monitors: candidate B, scheduler realm, chaos none, store json:

|   n |             startup ms | idle RSS MB |    RSS median MB |         CPU ms/poll |    CPU % core |       poll ratio |    polls/monitor |  behind |         events/s |      p95 latency ms |      p99 latency ms | health failures |      drain ms | realm build ms | realm resets | widest spread |
| --: | ---------------------: | ----------: | ---------------: | ------------------: | ------------: | ---------------: | ---------------: | ------: | ---------------: | ------------------: | ------------------: | --------------: | ------------: | -------------: | -----------: | ------------- |
|   1 | 5018.1 [5018.1–5018.1] |           — | 15.0 [15.0–15.0] | 11.10 [11.10–11.10] | 3.7 [3.7–3.7] | 0.50 [0.50–0.50] | 1.00 [1.00–1.00] | 0 [0–0] | 6.67 [6.67–6.67] | 372.7 [372.7–372.7] | 395.5 [395.5–395.5] |         0 [0–0] | 2.5 [2.5–2.5] |  3.8 [3.8–3.8] |      0 [0–0] | —             |

100 monitors: candidate C, scheduler realm, engine djs, chaos none, store json:

|   n |             startup ms | idle RSS MB |    RSS median MB |         CPU ms/poll |    CPU % core |       poll ratio |    polls/monitor |  behind |         events/s |      p95 latency ms |      p99 latency ms | health failures |      drain ms | realm build ms | realm resets | widest spread |
| --: | ---------------------: | ----------: | ---------------: | ------------------: | ------------: | ---------------: | ---------------: | ------: | ---------------: | ------------------: | ------------------: | --------------: | ------------: | -------------: | -----------: | ------------- |
|   1 | 5018.8 [5018.8–5018.8] |           — | 19.9 [19.9–19.9] | 11.00 [11.00–11.00] | 3.7 [3.7–3.7] | 0.50 [0.50–0.50] | 1.00 [1.00–1.00] | 0 [0–0] | 6.67 [6.67–6.67] | 652.9 [652.9–652.9] | 666.3 [666.3–666.3] |         0 [0–0] | 2.8 [2.8–2.8] |  4.0 [4.0–4.0] |      0 [0–0] | —             |

250 monitors: candidate A, chaos none, store json:

|   n |             startup ms | idle RSS MB |       RSS median MB |         CPU ms/poll |       CPU % core |       poll ratio |    polls/monitor |  behind |            events/s |   p95 latency ms |      p99 latency ms | health failures |         drain ms | realm build ms | realm resets | widest spread |
| --: | ---------------------: | ----------: | ------------------: | ------------------: | ---------------: | ---------------: | ---------------: | ------: | ------------------: | ---------------: | ------------------: | --------------: | ---------------: | -------------: | -----------: | ------------- |
|   1 | 5086.8 [5086.8–5086.8] |           — | 223.5 [223.5–223.5] | 69.48 [69.48–69.48] | 57.9 [57.9–57.9] | 0.50 [0.50–0.50] | 1.00 [1.00–1.00] | 0 [0–0] | 16.67 [16.67–16.67] | 74.3 [74.3–74.3] | 190.0 [190.0–190.0] |         0 [0–0] | 19.9 [19.9–19.9] |              — |            — | —             |

250 monitors: candidate B, scheduler seq, chaos none, store json:

|   n |             startup ms | idle RSS MB |    RSS median MB |         CPU ms/poll |       CPU % core |       poll ratio |    polls/monitor |  behind |            events/s |         p95 latency ms |         p99 latency ms | health failures |      drain ms | realm build ms | realm resets | widest spread |
| --: | ---------------------: | ----------: | ---------------: | ------------------: | ---------------: | ---------------: | ---------------: | ------: | ------------------: | ---------------------: | ---------------------: | --------------: | ------------: | -------------: | -----------: | ------------- |
|   1 | 5068.8 [5068.8–5068.8] |           — | 20.2 [20.2–20.2] | 23.00 [23.00–23.00] | 19.2 [19.2–19.2] | 0.50 [0.50–0.50] | 1.00 [1.00–1.00] | 0 [0–0] | 16.67 [16.67–16.67] | 2551.7 [2551.7–2551.7] | 2706.7 [2706.7–2706.7] |         0 [0–0] | 3.2 [3.2–3.2] |  3.3 [3.3–3.3] |      0 [0–0] | —             |

250 monitors: candidate B, scheduler realm, chaos none, store json:

|   n |             startup ms | idle RSS MB |    RSS median MB |         CPU ms/poll |       CPU % core |       poll ratio |    polls/monitor |  behind |            events/s |         p95 latency ms |         p99 latency ms | health failures |      drain ms | realm build ms | realm resets | widest spread |
| --: | ---------------------: | ----------: | ---------------: | ------------------: | ---------------: | ---------------: | ---------------: | ------: | ------------------: | ---------------------: | ---------------------: | --------------: | ------------: | -------------: | -----------: | ------------- |
|   1 | 5071.1 [5071.1–5071.1] |           — | 24.2 [24.2–24.2] | 23.40 [23.40–23.40] | 19.5 [19.5–19.5] | 0.50 [0.50–0.50] | 1.00 [1.00–1.00] | 0 [0–0] | 16.67 [16.67–16.67] | 2228.4 [2228.4–2228.4] | 2314.0 [2314.0–2314.0] |         0 [0–0] | 4.0 [4.0–4.0] |  4.0 [4.0–4.0] |      0 [0–0] | —             |

250 monitors: candidate C, scheduler realm, engine djs, chaos none, store json:

|   n |             startup ms | idle RSS MB |    RSS median MB |         CPU ms/poll |       CPU % core |       poll ratio |    polls/monitor |  behind |            events/s |         p95 latency ms |         p99 latency ms | health failures |      drain ms | realm build ms | realm resets | widest spread |
| --: | ---------------------: | ----------: | ---------------: | ------------------: | ---------------: | ---------------: | ---------------: | ------: | ------------------: | ---------------------: | ---------------------: | --------------: | ------------: | -------------: | -----------: | ------------- |
|   1 | 5071.8 [5071.8–5071.8] |           — | 29.8 [29.8–29.8] | 22.80 [22.80–22.80] | 19.0 [19.0–19.0] | 0.50 [0.50–0.50] | 1.00 [1.00–1.00] | 0 [0–0] | 16.67 [16.67–16.67] | 5041.0 [5041.0–5041.0] | 5505.7 [5505.7–5505.7] |         0 [0–0] | 4.3 [4.3–4.3] |  4.3 [4.3–4.3] |      0 [0–0] | —             |

500 monitors: candidate A, chaos none, store json:

|   n |             startup ms | idle RSS MB |       RSS median MB |            CPU ms/poll |          CPU % core |       poll ratio |    polls/monitor |  behind |            events/s |      p95 latency ms |         p99 latency ms | health failures |               drain ms | realm build ms | realm resets | widest spread |
| --: | ---------------------: | ----------: | ------------------: | ---------------------: | ------------------: | ---------------: | ---------------: | ------: | ------------------: | ------------------: | ---------------------: | --------------: | ---------------------: | -------------: | -----------: | ------------- |
|   1 | 5089.7 [5089.7–5089.7] |           — | 314.5 [314.5–314.5] | 117.45 [117.45–117.45] | 112.0 [112.0–112.0] | 0.29 [0.29–0.29] | 0.57 [0.57–0.57] | 1 [1–1] | 17.20 [17.20–17.20] | 216.0 [216.0–216.0] | 1783.3 [1783.3–1783.3] |         0 [0–0] | 3034.4 [3034.4–3034.4] |              — |            — | —             |

500 monitors: candidate B, scheduler seq, chaos none, store json:

|   n |             startup ms | idle RSS MB |    RSS median MB |         CPU ms/poll |       CPU % core |       poll ratio |    polls/monitor |  behind |            events/s |         p95 latency ms |         p99 latency ms | health failures |      drain ms | realm build ms | realm resets | widest spread |
| --: | ---------------------: | ----------: | ---------------: | ------------------: | ---------------: | ---------------: | ---------------: | ------: | ------------------: | ---------------------: | ---------------------: | --------------: | ------------: | -------------: | -----------: | ------------- |
|   1 | 5245.6 [5245.6–5245.6] |           — | 30.6 [30.6–30.6] | 40.70 [40.70–40.70] | 67.8 [67.8–67.8] | 0.50 [0.50–0.50] | 1.00 [1.00–1.00] | 0 [0–0] | 13.00 [13.00–13.00] | 9842.4 [9842.4–9842.4] | 9986.2 [9986.2–9986.2] |         0 [0–0] | 5.4 [5.4–5.4] |  3.3 [3.3–3.3] |      0 [0–0] | —             |

500 monitors: candidate B, scheduler realm, chaos none, store json:

|   n |             startup ms | idle RSS MB |    RSS median MB |         CPU ms/poll |       CPU % core |       poll ratio |    polls/monitor |  behind |            events/s |         p95 latency ms |         p99 latency ms | health failures |      drain ms | realm build ms | realm resets | widest spread |
| --: | ---------------------: | ----------: | ---------------: | ------------------: | ---------------: | ---------------: | ---------------: | ------: | ------------------: | ---------------------: | ---------------------: | --------------: | ------------: | -------------: | -----------: | ------------- |
|   1 | 5244.3 [5244.3–5244.3] |           — | 41.5 [41.5–41.5] | 40.06 [40.06–40.06] | 66.8 [66.8–66.8] | 0.50 [0.50–0.50] | 1.00 [1.00–1.00] | 0 [0–0] | 13.87 [13.87–13.87] | 9843.2 [9843.2–9843.2] | 9973.6 [9973.6–9973.6] |         0 [0–0] | 6.1 [6.1–6.1] |  4.2 [4.2–4.2] |      0 [0–0] | —             |

500 monitors: candidate C, scheduler realm, engine djs, chaos none, store json:

|   n |             startup ms | idle RSS MB |    RSS median MB |         CPU ms/poll |       CPU % core |       poll ratio |    polls/monitor |  behind |            events/s |         p95 latency ms |         p99 latency ms | health failures |      drain ms | realm build ms | realm resets | widest spread |
| --: | ---------------------: | ----------: | ---------------: | ------------------: | ---------------: | ---------------: | ---------------: | ------: | ------------------: | ---------------------: | ---------------------: | --------------: | ------------: | -------------: | -----------: | ------------- |
|   1 | 5254.5 [5254.5–5254.5] |           — | 35.1 [35.1–35.1] | 41.28 [41.28–41.28] | 68.8 [68.8–68.8] | 0.50 [0.50–0.50] | 1.00 [1.00–1.00] | 0 [0–0] | 18.00 [18.00–18.00] | 9839.1 [9839.1–9839.1] | 9998.2 [9998.2–9998.2] |         0 [0–0] | 5.7 [5.7–5.7] |  4.8 [4.8–4.8] |      0 [0–0] | —             |

1000 monitors: candidate A, chaos none, store json:

|   n |             startup ms | idle RSS MB |       RSS median MB |            CPU ms/poll |          CPU % core |       poll ratio |    polls/monitor |  behind |            events/s |         p95 latency ms |         p99 latency ms | health failures |               drain ms | realm build ms | realm resets | widest spread |
| --: | ---------------------: | ----------: | ------------------: | ---------------------: | ------------------: | ---------------: | ---------------: | ------: | ------------------: | ---------------------: | ---------------------: | --------------: | ---------------------: | -------------: | -----------: | ------------- |
|   1 | 5099.1 [5099.1–5099.1] |           — | 366.3 [366.3–366.3] | 147.13 [147.13–147.13] | 126.5 [126.5–126.5] | 0.13 [0.13–0.13] | 0.26 [0.26–0.26] | 1 [1–1] | 14.90 [14.90–14.90] | 1591.1 [1591.1–1591.1] | 2010.1 [2010.1–2010.1] |         0 [0–0] | 3033.3 [3033.3–3033.3] |              — |            — | —             |

1000 monitors: candidate B, scheduler seq, chaos none, store json:

|   n |             startup ms | idle RSS MB |    RSS median MB |         CPU ms/poll |          CPU % core |       poll ratio |    polls/monitor |  behind |         events/s | p95 latency ms | p99 latency ms | health failures |      drain ms | realm build ms | realm resets | widest spread |
| --: | ---------------------: | ----------: | ---------------: | ------------------: | ------------------: | ---------------: | ---------------: | ------: | ---------------: | -------------: | -------------: | --------------: | ------------: | -------------: | -----------: | ------------- |
|   1 | 5955.1 [5955.1–5955.1] |           — | 36.2 [36.2–36.2] | 59.60 [59.60–59.60] | 100.5 [100.5–100.5] | 0.25 [0.25–0.25] | 0.51 [0.51–0.51] | 1 [1–1] | 0.00 [0.00–0.00] |              — |              — |         0 [0–0] | 5.7 [5.7–5.7] |  3.4 [3.4–3.4] |      0 [0–0] | —             |

1000 monitors: candidate B, scheduler realm, chaos none, store json:

|   n |             startup ms | idle RSS MB |    RSS median MB |         CPU ms/poll |          CPU % core |       poll ratio |    polls/monitor |  behind |         events/s | p95 latency ms | p99 latency ms | health failures |      drain ms | realm build ms | realm resets | widest spread |
| --: | ---------------------: | ----------: | ---------------: | ------------------: | ------------------: | ---------------: | ---------------: | ------: | ---------------: | -------------: | -------------: | --------------: | ------------: | -------------: | -----------: | ------------- |
|   1 | 5976.0 [5976.0–5976.0] |           — | 47.8 [47.8–47.8] | 60.68 [60.68–60.68] | 100.7 [100.7–100.7] | 0.25 [0.25–0.25] | 0.50 [0.50–0.50] | 1 [1–1] | 0.00 [0.00–0.00] |              — |              — |         0 [0–0] | 6.2 [6.2–6.2] |  3.5 [3.5–3.5] |      0 [0–0] | —             |

1000 monitors: candidate C, scheduler realm, engine djs, chaos none, store json:

|   n |             startup ms | idle RSS MB |    RSS median MB |         CPU ms/poll |          CPU % core |       poll ratio |    polls/monitor |  behind |         events/s | p95 latency ms | p99 latency ms | health failures |      drain ms | realm build ms | realm resets | widest spread |
| --: | ---------------------: | ----------: | ---------------: | ------------------: | ------------------: | ---------------: | ---------------: | ------: | ---------------: | -------------: | -------------: | --------------: | ------------: | -------------: | -----------: | ------------- |
|   1 | 5991.6 [5991.6–5991.6] |           — | 39.3 [39.3–39.3] | 61.40 [61.40–61.40] | 101.1 [101.1–101.1] | 0.25 [0.25–0.25] | 0.49 [0.49–0.49] | 1 [1–1] | 0.00 [0.00–0.00] |              — |              — |         0 [0–0] | 5.5 [5.5–5.5] |  4.0 [4.0–4.0] |      0 [0–0] | —             |

### Package and port cost

Candidate A is the sourcefed tree as published — a 6.07 MB npm tarball and 6.45 MB of built `dist` — plus the Node 26 runtime it needs: Homebrew's `node` 26.13.1 cell is 77 MB, of which `libnode` alone is 61 MB. Candidate B is one stripped binary of 3,048,824 bytes (3.05 MB); candidate C's stripped binary is 3,625,272 bytes, 19% larger, so the djs engine costs 576 KB more than QuickJS in the same stall. Both load the same two plugin bundles, 68 KB in all.

What a port costs is the machinery, not the providers. The Rust stall is 3,028 lines of Rust source, 174 of them the realm prelude's JavaScript, plus 1,594 lines of Rust tests. What it replaces, counted as non-blank lines without comment-only lines, is `packages/core` 1,216 and `packages/daemon` 1,129 — 2,345 lines of machinery — while the providers' 3,005 lines do not need porting at all: the spike runs the pinned provider code unchanged as plugins in either engine. Candidate A's own TypeScript glue for the spike is 549 lines.

### Verdict

- Ready to serve: 2.5 ms (candidate B) and 2.6 ms (candidate C) against 88.5 ms at 10 monitors and 91.9 ms with no monitors for candidate A — about thirty times faster to answer `stall.start`, although the first poll lands on the 5 s tick either way.
- Memory: 70.4 MB idle for candidate A against 7.9 MB with one realm per type and 6.0 MB sequential, and 320.6 MB against 47.3 and 41.1 MB at 500 monitors — nine to twelve times smaller idle, seven times smaller loaded, with candidate C at 48.4 MB.
- CPU per poll: candidate B costs 2.2 times less at 10 monitors (5.00 against 11.00 ms), 3.4 times less at 100 (12.25 against 41.20) and 3.3 times less at 500 (34.01 against 112.46) with sourcefed's JSON store, and 21 and 53 times less with the store out of the way (2.00 against 41.50 ms at 100 monitors, 2.74 against 144.66 at 500).
- Capacity: with the store out of the way at 500 monitors, candidate B keeps every poll at 4.6% of a core and a p95 of 102.9 ms (41.0 ms sequential), where candidate A manages 0.45 polls a monitor at 107.5% of one core and a p95 of 2.10 s.
- Deal-breaker: one plugin that loops synchronously. Candidate A delivers nothing, stops answering health probes and is killed at its 10 s drain timeout; candidate B answers all 33 probes, keeps the other 20 monitors' events at 1.33 a second and drains in about 2 ms.

(The ready-to-serve figures are the medians of `stallStartMs` in the steady-10 and idle run JSONs, and the 33 probes are the fault-loop runs' `health` block.)

That last line is about the isolation model, not about Node. A Node stall could hold the same bound by putting each plugin in a worker thread, but candidate A as sourcefed specifies it today has none, so one synchronous loop in one provider takes the whole stall with it. A Rust stall with one realm per source type gets the bound from its structure: the poll deadline stops the realm, the realm is reset, and the rest of the stall keeps polling, whichever engine it was built with.

### Verdict, B against C

1. Memory: djs holds more, by 3.4 MB at 100 monitors (21.5 against 18.1 MB), 1.1 MB at 500 with the JSON store (48.4 against 47.3) and 3.9 MB at 500 with the store out of the way (23.0 against 19.1), and its idle footprint with no realm built is identical at 7.9 MB.
2. CPU per poll: a wash, within 4% everywhere (12.75 against 12.25 ms at 100 monitors, 34.07 against 34.01 at 500, 2.30 against 2.00 and 3.04 against 2.74 with the store out of the way), and the same events a second (8.89 and 29.69 at 100 and 500).
3. Realm cost: djs builds a realm 0.7–1.1 ms slower (5.0 against 4.3 ms at 100 monitors, 4.4 against 3.3 at 500) but comes back from a runaway poll with a ~0 ms reset against 2.8–2.9 ms to rebuild a QuickJS realm (both are the fault-loop runs' `engineTimings.resetMs`).
4. Containment: both bound a looping plugin; djs costs 853 ms of CPU per completed poll while it does so against 903 ms, and both keep delivering (1.33 events/s) with no health failure.
5. Neither is a performance reason to choose: C's binary is 19% larger and its tail latency is mixed (11.9 s against 14.5 s at 500 monitors with the JSON store, 76.1 against 102.9 ms with the store out of the way, 38.4 against 37.3 ms at 100), so the choice turns on the model and the risk, not on speed.

barn-plugin-host should start with djs behind its own trait, because the host-call boundary is the shape barn needs — an explicit, answerable, budgeted call rather than a blocking closure — the engine has no `unsafe` in its own code, and the switch costs nothing measurable. Keep the trait and keep the rquickjs implementation compiling behind its feature for at least one release, so the engine stays a build-time choice and a second engine can follow; treat the git pin, the single upstream and the 0.x API as the price of that choice, to be revisited when djs reaches a release.

## Caveats

- The GitHub rate limiter is lifted in candidate A's vendored copy (see the deviations above), so no figure here reflects sourcefed's shipped ceiling of 30 requests a minute, or the budget accounting that will replace it.
- Credentials are placeholders: the providers' auth path is exercised, secret handling is not, because barn owns it.
- The mock upstream is a loopback stand-in. There is no TLS, no real API latency, no upstream throttling and no rate-limit backoff, and every payload is 100 issues of the same shape. A real GitHub or Jira answer is slower and more variable, which would lower every events/s figure and raise every latency figure.
- The 15 s interval stands in for sourcefed's 60 s default, so the real load is four times lighter than what is measured here. That is why every figure is given per poll and per second: those survive the change of interval, the totals do not.
- barn's own surface is not measured at all. Its sandbox, listener handover, secret handling and budgets are what this spike assumes; a stall's cost under them is still unknown, and none of these runs had a supervisor in the loop.
- Candidate A re-lists its monitors for every event delivery. That is a real cost of today's daemon that a port would drop, so candidate A's CPU per poll is, if anything, generous to candidate A.
- Candidates B and C share a store that deliberately mirrors sourcefed's whole-file store, field for field, including the re-read. That is what makes their 500-monitor tail latency 14.5 s with one realm per type, 11.5 s sequential and 11.9 s on C, and why the memory-store tables are the fair reading of the runtime alone: with the store gone, the p95 at 500 monitors is 102.9 ms on B and 76.1 ms on C.
- The two Rust scheduler modes are not settled here. At these sizes the store dominates, and the two land within noise of each other on polls, CPU and memory; one realm per type is slower to deliver (102.9 ms against 41.0 ms p95 at 500 monitors with the memory store), a latency difference rather than a capacity one.
- The ramp is one run per size. It is a threshold probe that finds where polls start falling behind, not a like-for-like comparison.
- Each scenario, and each ramp size, ran in its own job on its own runner, so sizes are not compared across machines. One job ran a group's four variants: Xeon Platinum 8370C at 10 monitors, EPYC 7763 at 100 monitors, the storage-isolated and quiet runs and idle, Xeon 6973P-C at 500, EPYC 9V74 for the faults and the ramp.
- Run-to-run noise is in every table's `widest spread` column. The widest cells are p95 latency in the looping-plugin group, 2,769.2% of its median on the sequential runs and 1,136.1% with one realm per type, because one 10 s deadline over a handful of events dominates; p99 latency on the 10-monitor steady runs is 50.2–95.6% of its median. The 5-run tables are noisier than the 10-run ones (p95 627.2% on the sequential throwing-plugin runs, 425.6% on the djs looping runs), and the idle CPU cell reads 200% of a 10 ms median over a 0–20 ms range, which is a near-zero median rather than a real swing.
- Delivery is at-least-once: an event re-delivered after an SSE reconnect is counted again, which is why candidate A's events/s can exceed the events its polls actually saw.
- Linux only: process-tree samples come from `/proc`, CPU ticks are assumed at 100 Hz, and a run cut short by the health probe still divides by its configured window.
- djs's figures are one commit of one crate's engine — `djs-embed` at `0e58f62c80252fc3f4156ab7a3744746434ed860` — so they are a statement about that commit, not about djs as it will be.
- The two engines share the prelude and the bundles, so nothing here says how their JavaScript semantics differ. test262 coverage is djs's own claim, not measured here.
- In this run candidate A's 500-monitor steady figures came out better than in the previous run (1.01 polls a monitor and 29.87 events a second against 0.84 and 20.84, and 320.6 MB against 323.7 MB a run earlier), which is runner variance on a shared machine, not a change to A.

## How to re-run it

```sh
cd spike/stall-runtime
npm install && npm run build          # vendor sourcefed v0.5.2 and bundle the plugins
cargo build --release --manifest-path rust/Cargo.toml   # stall-b, the rquickjs engine
cp rust/target/release/stall-b rust/target/release/stall-b-rquickjs
cargo build --release --no-default-features --features engine-djs --manifest-path rust/Cargo.toml
cp rust/target/release/stall-b rust/target/release/stall-c
mv rust/target/release/stall-b-rquickjs rust/target/release/stall-b
node scripts/matrix.mjs --variants ts,rust-seq,rust-realm,rust-djs --scenario steady \
  --monitors 100 --runs 10 --interval 15 --window 45 --out results/steady-100
node harness/aggregate.mjs --dir results/steady-100 --out results/steady-100/summary.md
```

Or push to this branch: `.github/workflows/stall-runtime-spike.yml` builds all three candidates and runs every scenario. `mode: smoke` does one short run of each; `mode: full` does the whole matrix. Nothing in this spike has to run on the owner's Mac.
