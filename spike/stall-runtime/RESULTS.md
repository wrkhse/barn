# Spike: sourcefed's stall runtime, TypeScript on Node against Rust

**Status: measured.** Every figure below is a median from the aggregate of the run JSONs that `.github/workflows/stall-runtime-spike.yml` produced on GitHub Actions `ubuntu-latest`; the raw JSON is in the `stall-spike-results` artifact of that run. The figures are from workflow run 44104115314 (workflow "Stall runtime spike") on commit `5af5a97`, and any later run of the same workflow reproduces the same matrix. This branch is throwaway spike code. It is never merged into main. It exists to answer one question for the owner: should sourcefed's stall (its daemon under barn) stay TypeScript on Node 26, or be rewritten in Rust with source plugins in QuickJS realms?

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
- Priming: no window starts until every monitor has made one request, capped at 45 s. Both candidates poll on their first 5 s tick, so priming is set by that tick rather than by the runtime; a fault run's chaos monitor is exempt, because both candidates inject the fault before any broker call, so it never makes one. Every poll counted in a window is therefore a post-priming poll, and the two candidates are compared on the same cadence.
- Floor: a candidate that kept up polls each monitor floor(window ÷ interval) − 1 to floor(window ÷ interval) times inside the window. A run below that floor is marked `behind`, and the column carries 1 when it happened.
- Window: 45 s for the steady runs, 30 s for the quiet and fault runs, 20 s for idle, and 30 s per size in the ramp, each after a 2 s warm-up.
- Runs: 10 per candidate per size for the steady and idle scenarios, 5 for quiet, the storage-isolated runs and the fault runs, and 1 per size in the ramp, interleaved A, B(seq), B(realm), A, B(seq), B(realm) ... so slow drift on a shared runner hits all three alike.
- Machine: GitHub Actions `ubuntu-latest` (4 vCPU, 16 GiB). Same machine within a job, so each A-against-B comparison is same-machine; every scenario and every ramp size ran in its own job on its own runner, so sizes are not compared across machines, and each table names the CPU of the job it came from.

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
| Package size      | candidate A: the vendored build's `dist` bytes plus the published package's packed size plus the Node 26 binary it needs; candidate B: the stripped release binary                                                                                                                                                                                |
| Port cost         | non-blank source lines of the TypeScript that a port has to reproduce, per package, counted from the pinned tree                                                                                                                                                                                                                                  |

Every value in the tables is a median over the runs a group holds, with the range across them in brackets; a metric with no sample is left out of its cell, and a cell is — when no run had a sample.

## Results

Each table below is the harness's own aggregate output for one group of runs, copied as measured. `n` is how many runs the row holds; `widest spread` names the column whose run-to-run range was widest and gives that range as a percentage of its median. `behind` is 1 for a run below the priming floor, and a group's cell is the median of those 0/1 values, so 0 [0–1] means some runs fell below it and some did not. CPU % core can exceed 100% when the process tree uses several cores. All of it is on GitHub Actions `ubuntu-latest`, in the same job per comparison.

### Steady state, 10 monitors

At 10 monitors all three keep every poll, and the only wide differences are footprint and CPU: candidate A holds 79.1 MB against 11.3 MB (one realm per type) and 9.1 MB (sequential), and spends 8.50 ms of CPU a poll against 3.00 and 3.50.

Candidate B, scheduler realm, chaos none, store json:

|   n |             startup ms | idle RSS MB |    RSS median MB |      CPU ms/poll |    CPU % core |       poll ratio |    polls/monitor |  behind |         events/s | p95 latency ms | p99 latency ms | health failures |      drain ms | widest spread         |
| --: | ---------------------: | ----------: | ---------------: | ---------------: | ------------: | ---------------: | ---------------: | ------: | ---------------: | -------------: | -------------: | --------------: | ------------: | --------------------- |
|  10 | 5008.1 [5007.4–5008.6] |           — | 11.3 [11.2–11.4] | 3.00 [3.00–3.50] | 0.1 [0.1–0.2] | 0.67 [0.67–0.67] | 2.00 [2.00–2.00] | 0 [0–0] | 0.89 [0.89–0.89] | 5.2 [4.2–13.5] | 5.4 [4.3–14.2] |         0 [0–0] | 2.0 [1.8–5.3] | p99 latency ms 184.2% |

Candidate B, scheduler seq, chaos none, store json:

|   n |             startup ms | idle RSS MB | RSS median MB |      CPU ms/poll |    CPU % core |       poll ratio |    polls/monitor |  behind |         events/s | p95 latency ms | p99 latency ms | health failures |      drain ms | widest spread         |
| --: | ---------------------: | ----------: | ------------: | ---------------: | ------------: | ---------------: | ---------------: | ------: | ---------------: | -------------: | -------------: | --------------: | ------------: | --------------------- |
|  10 | 5008.0 [5007.4–5009.0] |           — | 9.1 [9.0–9.3] | 3.50 [3.00–3.50] | 0.2 [0.1–0.2] | 0.67 [0.67–0.67] | 2.00 [2.00–2.00] | 0 [0–0] | 0.89 [0.89–0.89] |  2.3 [2.1–6.9] |  2.6 [2.2–8.2] |         0 [0–0] | 1.9 [1.7–2.2] | p99 latency ms 231.2% |

Candidate A, chaos none, store json:

|   n |             startup ms | idle RSS MB |    RSS median MB |       CPU ms/poll |    CPU % core |       poll ratio |    polls/monitor |  behind |         events/s | p95 latency ms | p99 latency ms | health failures |       drain ms | widest spread         |
| --: | ---------------------: | ----------: | ---------------: | ----------------: | ------------: | ---------------: | ---------------: | ------: | ---------------: | -------------: | -------------: | --------------: | -------------: | --------------------- |
|  10 | 5073.4 [5068.1–5080.9] |           — | 79.1 [78.9–80.0] | 8.50 [7.00–11.00] | 0.4 [0.3–0.5] | 0.67 [0.67–0.67] | 2.00 [2.00–2.00] | 0 [0–0] | 0.89 [0.89–0.89] |  3.0 [2.2–9.1] | 7.2 [2.6–19.5] |         0 [0–0] | 7.2 [6.6–11.2] | p99 latency ms 233.5% |

### Steady state, 100 monitors

All three still keep every poll. Candidate A's footprint reaches 180.7 MB against 18.3 and 15.7 MB and its CPU per poll 39.40 ms against 12.35 and 12.30, yet its p95 latency, 10.3 ms, is now the lower one, against 423.4 and 366.7 ms: at this size the serialised whole-file store, not the runtime, is what delivery waits behind.

Candidate B, scheduler realm, chaos none, store json:

|   n |             startup ms | idle RSS MB |    RSS median MB |         CPU ms/poll |    CPU % core |       poll ratio |    polls/monitor |  behind |         events/s |      p95 latency ms |      p99 latency ms | health failures |      drain ms | widest spread        |
| --: | ---------------------: | ----------: | ---------------: | ------------------: | ------------: | ---------------: | ---------------: | ------: | ---------------: | ------------------: | ------------------: | --------------: | ------------: | -------------------- |
|  10 | 5023.6 [5022.8–5024.6] |           — | 18.3 [17.9–18.4] | 12.35 [12.15–12.65] | 5.5 [5.4–5.6] | 0.67 [0.67–0.67] | 2.00 [2.00–2.00] | 0 [0–0] | 8.89 [8.89–8.89] | 423.4 [411.0–495.3] | 449.8 [420.3–542.4] |         0 [0–0] | 3.4 [3.2–4.0] | p99 latency ms 27.2% |

Candidate B, scheduler seq, chaos none, store json:

|   n |             startup ms | idle RSS MB |    RSS median MB |         CPU ms/poll |    CPU % core |       poll ratio |    polls/monitor |  behind |         events/s |      p95 latency ms |      p99 latency ms | health failures |      drain ms | widest spread  |
| --: | ---------------------: | ----------: | ---------------: | ------------------: | ------------: | ---------------: | ---------------: | ------: | ---------------: | ------------------: | ------------------: | --------------: | ------------: | -------------- |
|  10 | 5023.5 [5023.0–5024.3] |           — | 15.7 [13.8–15.8] | 12.30 [12.20–12.60] | 5.5 [5.4–5.6] | 0.67 [0.67–0.67] | 2.00 [2.00–2.00] | 0 [0–0] | 8.89 [8.89–8.89] | 366.7 [353.2–379.5] | 396.2 [383.2–411.4] |         0 [0–0] | 3.3 [2.8–3.7] | drain ms 25.6% |

Candidate A, chaos none, store json:

|   n |             startup ms | idle RSS MB |       RSS median MB |         CPU ms/poll |       CPU % core |       poll ratio |    polls/monitor |  behind |         events/s |  p95 latency ms |   p99 latency ms | health failures |         drain ms | widest spread        |
| --: | ---------------------: | ----------: | ------------------: | ------------------: | ---------------: | ---------------: | ---------------: | ------: | ---------------: | --------------: | ---------------: | --------------: | ---------------: | -------------------- |
|  10 | 5105.9 [5099.6–5113.4] |           — | 180.7 [176.8–188.5] | 39.40 [38.40–42.65] | 17.5 [17.1–19.0] | 0.67 [0.67–0.67] | 2.00 [2.00–2.00] | 0 [0–0] | 8.89 [8.89–8.89] | 10.3 [9.7–12.8] | 14.7 [13.6–16.2] |         0 [0–0] | 19.0 [17.7–19.7] | p95 latency ms 30.6% |

### Steady state, 500 monitors

Here the runtime and the store stop being separable. Priming at this size costs candidate B 5.5 s, and candidate A reaches the harness's 45 s cap. Candidate A then falls behind (0.84 polls a monitor, about nine polls a second, `behind` 1, 20.8 events/s) while both Rust schedulers keep every poll (2.00 polls a monitor, 23.6 events/s) in 46.0 and 37.9 MB against A's 323.7 MB — but delivery now queues behind the store, so the Rust p95 is about 20 s against A's 1.57 s. That figure is a statement about the store, and the memory-store tables below are the reading of the runtime without it.

Candidate B, scheduler realm, chaos none, store json:

|   n |             startup ms | idle RSS MB |    RSS median MB |         CPU ms/poll |       CPU % core |       poll ratio |    polls/monitor |  behind |            events/s |            p95 latency ms |            p99 latency ms | health failures |      drain ms | widest spread  |
| --: | ---------------------: | ----------: | ---------------: | ------------------: | ---------------: | ---------------: | ---------------: | ------: | ------------------: | ------------------------: | ------------------------: | --------------: | ------------: | -------------- |
|  10 | 5305.7 [5304.3–5309.0] |           — | 46.0 [39.9–47.7] | 39.88 [39.31–42.35] | 87.8 [87.4–89.0] | 0.67 [0.62–0.67] | 2.00 [1.86–2.00] | 0 [0–1] | 23.56 [22.71–24.93] | 20191.2 [19715.4–20782.2] | 20569.2 [20126.3–21143.7] |         0 [0–0] | 7.4 [6.5–7.9] | drain ms 19.6% |

Candidate B, scheduler seq, chaos none, store json:

|   n |             startup ms | idle RSS MB |    RSS median MB |         CPU ms/poll |       CPU % core |       poll ratio |    polls/monitor |  behind |            events/s |            p95 latency ms |            p99 latency ms | health failures |      drain ms | widest spread  |
| --: | ---------------------: | ----------: | ---------------: | ------------------: | ---------------: | ---------------: | ---------------: | ------: | ------------------: | ------------------------: | ------------------------: | --------------: | ------------: | -------------- |
|  10 | 5305.8 [5305.2–5310.8] |           — | 37.9 [35.8–43.3] | 39.46 [39.36–39.92] | 87.7 [87.5–88.7] | 0.67 [0.67–0.67] | 2.00 [2.00–2.00] | 0 [0–1] | 23.51 [22.71–25.38] | 19975.3 [19545.3–20697.7] | 20469.4 [19984.5–21090.2] |         0 [0–0] | 6.5 [5.7–7.4] | drain ms 26.0% |

Candidate A, chaos none, store json:

|   n |             startup ms | idle RSS MB |       RSS median MB |            CPU ms/poll |          CPU % core |       poll ratio |    polls/monitor |  behind |            events/s |         p95 latency ms |         p99 latency ms | health failures |               drain ms | widest spread        |
| --: | ---------------------: | ----------: | ------------------: | ---------------------: | ------------------: | ---------------: | ---------------: | ------: | ------------------: | ---------------------: | ---------------------: | --------------: | ---------------------: | -------------------- |
|  10 | 5106.6 [5102.2–5114.7] |           — | 323.7 [313.0–331.0] | 134.88 [133.14–136.65] | 125.9 [125.1–126.5] | 0.28 [0.27–0.28] | 0.84 [0.82–0.85] | 1 [1–1] | 20.84 [19.84–25.49] | 1573.0 [1337.1–1808.1] | 2007.1 [1949.7–2068.3] |         0 [0–0] | 3035.4 [3031.1–3043.2] | p95 latency ms 29.9% |

### Storage isolated at 100 monitors

With the store out of the way candidate A costs 30.20 ms of CPU a poll against 1.50 and 1.30 — twenty to twenty-three times — and holds 151.0 MB against 12.6 and 10.2. Here, while both keep up, A's delivered-event p95 is the lower one: 17.5 ms against 32.9 and 36.6 ms.

Candidate B, scheduler realm, chaos none, store memory:

|   n |             startup ms | idle RSS MB |    RSS median MB |      CPU ms/poll |    CPU % core |       poll ratio |    polls/monitor |  behind |         events/s |   p95 latency ms |   p99 latency ms | health failures |      drain ms | widest spread        |
| --: | ---------------------: | ----------: | ---------------: | ---------------: | ------------: | ---------------: | ---------------: | ------: | ---------------: | ---------------: | ---------------: | --------------: | ------------: | -------------------- |
|   5 | 5008.0 [5007.5–5008.1] |           — | 12.6 [12.5–12.7] | 1.50 [1.50–1.50] | 0.5 [0.5–0.5] | 0.50 [0.50–0.50] | 1.00 [1.00–1.00] | 0 [0–0] | 6.67 [6.67–6.67] | 32.9 [28.5–38.3] | 38.8 [38.7–40.4] |         0 [0–0] | 2.2 [2.2–2.4] | p95 latency ms 29.7% |

Candidate B, scheduler seq, chaos none, store memory:

|   n |             startup ms | idle RSS MB |    RSS median MB |      CPU ms/poll |    CPU % core |       poll ratio |    polls/monitor |  behind |         events/s |   p95 latency ms |   p99 latency ms | health failures |      drain ms | widest spread  |
| --: | ---------------------: | ----------: | ---------------: | ---------------: | ------------: | ---------------: | ---------------: | ------: | ---------------: | ---------------: | ---------------: | --------------: | ------------: | -------------- |
|   5 | 5007.9 [5007.9–5009.0] |           — | 10.2 [10.1–10.3] | 1.30 [1.20–1.40] | 0.4 [0.4–0.5] | 0.50 [0.50–0.50] | 1.00 [1.00–1.00] | 0 [0–0] | 6.67 [6.67–6.67] | 36.6 [36.4–37.1] | 39.2 [39.1–39.7] |         0 [0–0] | 2.1 [1.8–2.2] | drain ms 19.5% |

Candidate A, chaos none, store memory:

|   n |             startup ms | idle RSS MB |       RSS median MB |         CPU ms/poll |       CPU % core |       poll ratio |    polls/monitor |  behind |         events/s |   p95 latency ms |   p99 latency ms | health failures |         drain ms | widest spread       |
| --: | ---------------------: | ----------: | ------------------: | ------------------: | ---------------: | ---------------: | ---------------: | ------: | ---------------: | ---------------: | ---------------: | --------------: | ---------------: | ------------------- |
|   5 | 5081.0 [5078.6–5089.3] |           — | 151.0 [148.5–151.5] | 30.20 [30.00–30.40] | 10.1 [10.0–10.1] | 0.50 [0.50–0.50] | 1.00 [1.00–1.00] | 0 [0–0] | 6.67 [6.67–6.67] | 17.5 [17.0–18.3] | 18.8 [17.9–19.2] |         0 [0–0] | 10.5 [10.0–10.7] | p95 latency ms 7.5% |

### Storage isolated at 500 monitors

At 500 monitors with the store gone the gap is 117.2 against 2.04 and 1.96 ms a poll, about 57 times, and it is a difference in capacity rather than in cost: candidate A needs 107.9% of one core and still reaches only 0.55 polls a monitor with a 1.23 s p95, while candidate B needs 3.4% of a core, keeps every poll and delivers at 85.5 ms (one realm per type) or 37.0 ms (sequential).

Candidate B, scheduler realm, chaos none, store memory:

|   n |             startup ms | idle RSS MB |    RSS median MB |      CPU ms/poll |    CPU % core |       poll ratio |    polls/monitor |  behind |            events/s |    p95 latency ms |    p99 latency ms | health failures |      drain ms | widest spread  |
| --: | ---------------------: | ----------: | ---------------: | ---------------: | ------------: | ---------------: | ---------------: | ------: | ------------------: | ----------------: | ----------------: | --------------: | ------------: | -------------- |
|   5 | 5008.4 [5008.4–5009.1] |           — | 19.1 [19.0–19.3] | 2.04 [2.02–2.08] | 3.4 [3.4–3.5] | 0.50 [0.50–0.50] | 1.00 [1.00–1.00] | 0 [0–0] | 33.33 [33.33–33.33] | 85.5 [71.9–101.0] | 94.2 [79.7–108.3] |         0 [0–0] | 3.6 [3.5–5.3] | drain ms 50.6% |

Candidate B, scheduler seq, chaos none, store memory:

|   n |             startup ms | idle RSS MB |    RSS median MB |      CPU ms/poll |    CPU % core |       poll ratio |    polls/monitor |  behind |            events/s |   p95 latency ms |   p99 latency ms | health failures |      drain ms | widest spread        |
| --: | ---------------------: | ----------: | ---------------: | ---------------: | ------------: | ---------------: | ---------------: | ------: | ------------------: | ---------------: | ---------------: | --------------: | ------------: | -------------------- |
|   5 | 5009.5 [5009.2–5010.0] |           — | 16.5 [16.5–16.7] | 1.96 [1.92–1.98] | 3.3 [3.2–3.3] | 0.50 [0.50–0.50] | 1.00 [1.00–1.00] | 0 [0–0] | 33.33 [33.33–33.33] | 37.0 [10.2–39.3] | 46.8 [24.1–48.2] |         0 [0–0] | 3.3 [3.0–3.9] | p95 latency ms 78.8% |

Candidate A, chaos none, store memory:

|   n |             startup ms | idle RSS MB |       RSS median MB |            CPU ms/poll |          CPU % core |       poll ratio |    polls/monitor |  behind |            events/s |         p95 latency ms |         p99 latency ms | health failures |               drain ms | widest spread        |
| --: | ---------------------: | ----------: | ------------------: | ---------------------: | ------------------: | ---------------: | ---------------: | ------: | ------------------: | ---------------------: | ---------------------: | --------------: | ---------------------: | -------------------- |
|   5 | 5079.9 [5078.4–5082.6] |           — | 202.9 [201.1–206.9] | 117.22 [114.95–124.52] | 107.9 [107.5–108.4] | 0.28 [0.26–0.28] | 0.55 [0.52–0.56] | 1 [1–1] | 18.07 [16.87–18.33] | 1233.5 [1100.7–1387.5] | 1480.9 [1265.4–1839.2] |         0 [0–0] | 3976.5 [3912.3–4127.2] | p99 latency ms 38.8% |

### Idle

With no monitors at all candidate A holds 70.4 MB against 7.9 MB with one realm per type and 6.0 MB sequential, so the floor of a Node process is nine to twelve times the floor of a single binary, before any monitor exists.

Candidate B, scheduler realm, chaos none, store json:

|   n | startup ms |   idle RSS MB | RSS median MB |      CPU ms/poll |    CPU % core | poll ratio | polls/monitor | behind |         events/s | p95 latency ms | p99 latency ms | health failures |      drain ms | widest spread  |
| --: | ---------: | ------------: | ------------: | ---------------: | ------------: | ---------: | ------------: | -----: | ---------------: | -------------: | -------------: | --------------: | ------------: | -------------- |
|  10 |          — | 7.9 [7.9–7.9] | 7.9 [7.9–7.9] | 0.00 [0.00–0.00] | 0.0 [0.0–0.0] |          — |             — |      — | 0.00 [0.00–0.00] |              — |              — |         0 [0–0] | 2.3 [2.0–2.5] | drain ms 23.2% |

Candidate B, scheduler seq, chaos none, store json:

|   n | startup ms |   idle RSS MB | RSS median MB |      CPU ms/poll |    CPU % core | poll ratio | polls/monitor | behind |         events/s | p95 latency ms | p99 latency ms | health failures |      drain ms | widest spread  |
| --: | ---------: | ------------: | ------------: | ---------------: | ------------: | ---------: | ------------: | -----: | ---------------: | -------------: | -------------: | --------------: | ------------: | -------------- |
|  10 |          — | 6.0 [5.9–6.1] | 6.0 [5.9–6.1] | 0.00 [0.00–0.00] | 0.0 [0.0–0.0] |          — |             — |      — | 0.00 [0.00–0.00] |              — |              — |         0 [0–0] | 2.0 [1.8–2.3] | drain ms 25.7% |

Candidate A, chaos none, store json:

|   n | startup ms |      idle RSS MB |    RSS median MB |        CPU ms/poll |    CPU % core | poll ratio | polls/monitor | behind |         events/s | p95 latency ms | p99 latency ms | health failures |      drain ms | widest spread      |
| --: | ---------: | ---------------: | ---------------: | -----------------: | ------------: | ---------: | ------------: | -----: | ---------------: | -------------: | -------------: | --------------: | ------------: | ------------------ |
|  10 |          — | 70.4 [70.3–70.5] | 70.4 [70.3–70.5] | 10.00 [0.00–20.00] | 0.1 [0.0–0.1] |          — |             — |      — | 0.00 [0.00–0.00] |              — |              — |         0 [0–0] | 8.4 [7.8–9.2] | CPU ms/poll 200.0% |

### One plugin throws

A throwing upstream is handled the same way by all three: 0.95 polls a monitor, 1.33 events a second, no health failure and a clean drain (2.5, 2.2 and 8.4 ms). Whatever separates the candidates, it is not error handling.

Candidate B, scheduler realm, chaos throw, store json:

|   n |             startup ms | idle RSS MB |    RSS median MB |      CPU ms/poll |    CPU % core |       poll ratio |    polls/monitor |  behind |         events/s |   p95 latency ms |   p99 latency ms | health failures |      drain ms | widest spread        |
| --: | ---------------------: | ----------: | ---------------: | ---------------: | ------------: | ---------------: | ---------------: | ------: | ---------------: | ---------------: | ---------------: | --------------: | ------------: | -------------------- |
|   5 | 5010.4 [5010.0–5011.8] |           — | 11.8 [11.5–11.9] | 6.00 [5.50–6.50] | 0.4 [0.4–0.4] | 0.48 [0.48–0.48] | 0.95 [0.95–0.95] | 1 [1–1] | 1.33 [1.33–1.33] | 16.4 [12.3–18.2] | 16.6 [12.7–18.8] |         0 [0–0] | 2.5 [2.4–2.6] | p99 latency ms 37.0% |

Candidate B, scheduler seq, chaos throw, store json:

|   n |             startup ms | idle RSS MB | RSS median MB |      CPU ms/poll |    CPU % core |       poll ratio |    polls/monitor |  behind |         events/s | p95 latency ms | p99 latency ms | health failures |      drain ms | widest spread         |
| --: | ---------------------: | ----------: | ------------: | ---------------: | ------------: | ---------------: | ---------------: | ------: | ---------------: | -------------: | -------------: | --------------: | ------------: | --------------------- |
|   5 | 5010.6 [5010.4–5010.8] |           — | 9.5 [9.5–9.8] | 6.00 [5.50–6.00] | 0.4 [0.4–0.4] | 0.48 [0.48–0.48] | 0.95 [0.95–0.95] | 1 [1–1] | 1.33 [1.33–1.33] |  3.5 [3.2–8.1] |  3.7 [3.2–8.3] |         0 [0–0] | 2.2 [2.1–2.3] | p95 latency ms 139.2% |

Candidate A, chaos throw, store json:

|   n |             startup ms | idle RSS MB |    RSS median MB |         CPU ms/poll |    CPU % core |       poll ratio |    polls/monitor |  behind |         events/s | p95 latency ms | p99 latency ms | health failures |      drain ms | widest spread        |
| --: | ---------------------: | ----------: | ---------------: | ------------------: | ------------: | ---------------: | ---------------: | ------: | ---------------: | -------------: | -------------: | --------------: | ------------: | -------------------- |
|   5 | 5101.8 [5100.2–5127.1] |           — | 89.0 [89.0–89.4] | 18.00 [17.50–18.00] | 1.2 [1.2–1.2] | 0.48 [0.48–0.48] | 0.95 [0.95–0.95] | 1 [1–1] | 1.33 [1.33–1.33] |  6.8 [3.9–7.1] |  8.3 [5.0–8.9] |         0 [0–0] | 8.4 [8.1–8.7] | p99 latency ms 46.8% |

### One plugin loops

Here the three stop being comparable. Candidate A delivers nothing for the whole 30 s window, stops answering health probes at 11.28 s (three failures, after which the harness marks it unresponsive) and never finishes `stall.drain`, so the harness kills it at its 10 s timeout — five runs, no spread. Candidate B keeps answering every health probe (33 of 33), keeps delivering the other 20 monitors' events (1.33 a second, p95 3.5 ms sequential and 14.1 ms with one realm per type) and drains in 2.6 and 2.4 ms; the looping realm is interrupted at its 10 s poll deadline and rebuilt, which costs about 906 ms of CPU per completed poll while it is rebuilt, about 60% of a core over the window.

Candidate B, scheduler realm, chaos loop, store json:

|   n |             startup ms | idle RSS MB |    RSS median MB |            CPU ms/poll |       CPU % core |       poll ratio |    polls/monitor |  behind |         events/s |   p95 latency ms |   p99 latency ms | health failures |      drain ms | widest spread        |
| --: | ---------------------: | ----------: | ---------------: | ---------------------: | ---------------: | ---------------: | ---------------: | ------: | ---------------: | ---------------: | ---------------: | --------------: | ------------: | -------------------- |
|   5 | 5010.7 [5010.4–5010.9] |           — | 11.9 [11.8–12.0] | 906.00 [905.50–906.50] | 60.4 [60.4–60.4] | 0.48 [0.48–0.48] | 0.95 [0.95–0.95] | 1 [1–1] | 1.33 [1.33–1.33] | 14.1 [11.1–16.4] | 14.5 [11.5–16.5] |         0 [0–0] | 2.6 [2.5–2.9] | p95 latency ms 37.6% |

Candidate B, scheduler seq, chaos loop, store json:

|   n |             startup ms | idle RSS MB | RSS median MB |            CPU ms/poll |       CPU % core |       poll ratio |    polls/monitor |  behind |         events/s | p95 latency ms | p99 latency ms | health failures |      drain ms | widest spread  |
| --: | ---------------------: | ----------: | ------------: | ---------------------: | ---------------: | ---------------: | ---------------: | ------: | ---------------: | -------------: | -------------: | --------------: | ------------: | -------------- |
|   5 | 5010.8 [5010.1–5011.1] |           — | 9.6 [9.2–9.7] | 905.50 [905.00–906.00] | 60.4 [60.3–60.4] | 0.48 [0.48–0.48] | 0.95 [0.95–0.95] | 1 [1–1] | 1.33 [1.33–1.33] |  3.5 [3.1–3.9] |  3.9 [3.2–4.2] |         0 [0–0] | 2.4 [2.1–2.7] | drain ms 27.6% |

Candidate A, chaos loop, store json:

|   n |             startup ms | idle RSS MB |    RSS median MB |               CPU ms/poll |       CPU % core |       poll ratio |    polls/monitor |  behind |         events/s | p95 latency ms | p99 latency ms | health failures |                  drain ms | widest spread      |
| --: | ---------------------: | ----------: | ---------------: | ------------------------: | ---------------: | ---------------: | ---------------: | ------: | ---------------: | -------------: | -------------: | --------------: | ------------------------: | ------------------ |
|   5 | 5102.9 [5098.9–5108.0] |           — | 88.2 [88.1–88.9] | 4010.00 [4010.00–4020.00] | 13.4 [13.4–13.4] | 0.00 [0.00–0.00] | 0.00 [0.00–0.00] | 1 [1–1] | 0.00 [0.00–0.00] |              — |              — |         3 [3–3] | 10012.1 [10011.6–10012.8] | RSS median MB 0.8% |

### Ramp: 100, 250, 500 and 1000 monitors

Both candidates hold the line to 250 monitors. At 500 candidate B still polls every monitor once in the 30 s window while candidate A manages 0.51, and at 1000 both are behind (B 0.40, A 0.22), where candidate A's priming also timed out with 363 of 1000 monitors primed. Candidate B's delivered-event figure at 1000 monitors is 0.00 and is not comparable: its per-client SSE queue holds 64 frames and dropped the fake host. In the 45 s steady runs the same crossing shows up as A's 0.84 polls a monitor against B's 2.00.

100 monitors: candidate B, scheduler realm, chaos none, store json:

|   n |             startup ms | idle RSS MB |    RSS median MB |         CPU ms/poll |    CPU % core |       poll ratio |    polls/monitor |  behind |         events/s |      p95 latency ms |      p99 latency ms | health failures |      drain ms | widest spread |
| --: | ---------------------: | ----------: | ---------------: | ------------------: | ------------: | ---------------: | ---------------: | ------: | ---------------: | ------------------: | ------------------: | --------------: | ------------: | ------------- |
|   1 | 5024.0 [5024.0–5024.0] |           — | 16.0 [16.0–16.0] | 14.00 [14.00–14.00] | 4.7 [4.7–4.7] | 0.50 [0.50–0.50] | 1.00 [1.00–1.00] | 0 [0–0] | 6.67 [6.67–6.67] | 429.7 [429.7–429.7] | 453.5 [453.5–453.5] |         0 [0–0] | 3.0 [3.0–3.0] | —             |

100 monitors: candidate B, scheduler seq, chaos none, store json:

|   n |             startup ms | idle RSS MB |    RSS median MB |         CPU ms/poll |    CPU % core |       poll ratio |    polls/monitor |  behind |         events/s |      p95 latency ms |      p99 latency ms | health failures |      drain ms | widest spread |
| --: | ---------------------: | ----------: | ---------------: | ------------------: | ------------: | ---------------: | ---------------: | ------: | ---------------: | ------------------: | ------------------: | --------------: | ------------: | ------------- |
|   1 | 5023.4 [5023.4–5023.4] |           — | 14.1 [14.1–14.1] | 14.00 [14.00–14.00] | 4.7 [4.7–4.7] | 0.50 [0.50–0.50] | 1.00 [1.00–1.00] | 0 [0–0] | 6.67 [6.67–6.67] | 358.2 [358.2–358.2] | 393.1 [393.1–393.1] |         0 [0–0] | 2.8 [2.8–2.8] | —             |

100 monitors: candidate A, chaos none, store json:

|   n |             startup ms | idle RSS MB |       RSS median MB |         CPU ms/poll |       CPU % core |       poll ratio |    polls/monitor |  behind |         events/s |   p95 latency ms |   p99 latency ms | health failures |         drain ms | widest spread |
| --: | ---------------------: | ----------: | ------------------: | ------------------: | ---------------: | ---------------: | ---------------: | ------: | ---------------: | ---------------: | ---------------: | --------------: | ---------------: | ------------- |
|   1 | 5107.1 [5107.1–5107.1] |           — | 174.3 [174.3–174.3] | 46.40 [46.40–46.40] | 15.5 [15.5–15.5] | 0.50 [0.50–0.50] | 1.00 [1.00–1.00] | 0 [0–0] | 6.67 [6.67–6.67] | 11.5 [11.5–11.5] | 14.9 [14.9–14.9] |         0 [0–0] | 15.7 [15.7–15.7] | —             |

250 monitors: candidate B, scheduler realm, chaos none, store json:

|   n |             startup ms | idle RSS MB |    RSS median MB |         CPU ms/poll |       CPU % core |       poll ratio |    polls/monitor |  behind |            events/s |         p95 latency ms |         p99 latency ms | health failures |      drain ms | widest spread |
| --: | ---------------------: | ----------: | ---------------: | ------------------: | ---------------: | ---------------: | ---------------: | ------: | ------------------: | ---------------------: | ---------------------: | --------------: | ------------: | ------------- |
|   1 | 5085.5 [5085.5–5085.5] |           — | 22.9 [22.9–22.9] | 29.40 [29.40–29.40] | 24.5 [24.5–24.5] | 0.50 [0.50–0.50] | 1.00 [1.00–1.00] | 0 [0–0] | 16.67 [16.67–16.67] | 2458.8 [2458.8–2458.8] | 2544.9 [2544.9–2544.9] |         0 [0–0] | 4.1 [4.1–4.1] | —             |

250 monitors: candidate B, scheduler seq, chaos none, store json:

|   n |             startup ms | idle RSS MB |    RSS median MB |         CPU ms/poll |       CPU % core |       poll ratio |    polls/monitor |  behind |            events/s |         p95 latency ms |         p99 latency ms | health failures |      drain ms | widest spread |
| --: | ---------------------: | ----------: | ---------------: | ------------------: | ---------------: | ---------------: | ---------------: | ------: | ------------------: | ---------------------: | ---------------------: | --------------: | ------------: | ------------- |
|   1 | 5087.7 [5087.7–5087.7] |           — | 24.4 [24.4–24.4] | 29.44 [29.44–29.44] | 24.5 [24.5–24.5] | 0.50 [0.50–0.50] | 1.00 [1.00–1.00] | 0 [0–0] | 16.67 [16.67–16.67] | 2387.0 [2387.0–2387.0] | 2483.1 [2483.1–2483.1] |         0 [0–0] | 4.4 [4.4–4.4] | —             |

250 monitors: candidate A, chaos none, store json:

|   n |             startup ms | idle RSS MB |       RSS median MB |         CPU ms/poll |       CPU % core |       poll ratio |    polls/monitor |  behind |            events/s |         p95 latency ms |         p99 latency ms | health failures |               drain ms | widest spread |
| --: | ---------------------: | ----------: | ------------------: | ------------------: | ---------------: | ---------------: | ---------------: | ------: | ------------------: | ---------------------: | ---------------------: | --------------: | ---------------------: | ------------- |
|   1 | 5107.2 [5107.2–5107.2] |           — | 232.7 [232.7–232.7] | 88.92 [88.92–88.92] | 77.1 [77.1–77.1] | 0.52 [0.52–0.52] | 1.04 [1.04–1.04] | 0 [0–0] | 20.07 [20.07–20.07] | 1284.0 [1284.0–1284.0] | 1865.9 [1865.9–1865.9] |         0 [0–0] | 3019.8 [3019.8–3019.8] | —             |

500 monitors: candidate B, scheduler realm, chaos none, store json:

|   n |             startup ms | idle RSS MB |    RSS median MB |         CPU ms/poll |       CPU % core |       poll ratio |    polls/monitor |  behind |            events/s |         p95 latency ms |         p99 latency ms | health failures |      drain ms | widest spread |
| --: | ---------------------: | ----------: | ---------------: | ------------------: | ---------------: | ---------------: | ---------------: | ------: | ------------------: | ---------------------: | ---------------------: | --------------: | ------------: | ------------- |
|   1 | 5306.9 [5306.9–5306.9] |           — | 39.0 [39.0–39.0] | 48.50 [48.50–48.50] | 80.8 [80.8–80.8] | 0.50 [0.50–0.50] | 1.00 [1.00–1.00] | 0 [0–0] | 13.07 [13.07–13.07] | 9415.8 [9415.8–9415.8] | 9571.3 [9571.3–9571.3] |         0 [0–0] | 6.6 [6.6–6.6] | —             |

500 monitors: candidate B, scheduler seq, chaos none, store json:

|   n |             startup ms | idle RSS MB |    RSS median MB |         CPU ms/poll |       CPU % core |       poll ratio |    polls/monitor |  behind |            events/s |         p95 latency ms |         p99 latency ms | health failures |      drain ms | widest spread |
| --: | ---------------------: | ----------: | ---------------: | ------------------: | ---------------: | ---------------: | ---------------: | ------: | ------------------: | ---------------------: | ---------------------: | --------------: | ------------: | ------------- |
|   1 | 5307.3 [5307.3–5307.3] |           — | 32.3 [32.3–32.3] | 48.50 [48.50–48.50] | 80.8 [80.8–80.8] | 0.50 [0.50–0.50] | 1.00 [1.00–1.00] | 0 [0–0] | 13.20 [13.20–13.20] | 9439.4 [9439.4–9439.4] | 9583.0 [9583.0–9583.0] |         0 [0–0] | 5.4 [5.4–5.4] | —             |

500 monitors: candidate A, chaos none, store json:

|   n |             startup ms | idle RSS MB |       RSS median MB |            CPU ms/poll |          CPU % core |       poll ratio |    polls/monitor |  behind |            events/s |         p95 latency ms |         p99 latency ms | health failures |               drain ms | widest spread |
| --: | ---------------------: | ----------: | ------------------: | ---------------------: | ------------------: | ---------------: | ---------------: | ------: | ------------------: | ---------------------: | ---------------------: | --------------: | ---------------------: | ------------- |
|   1 | 5110.5 [5110.5–5110.5] |           — | 319.7 [319.7–319.7] | 146.29 [146.29–146.29] | 124.8 [124.8–124.8] | 0.26 [0.26–0.26] | 0.51 [0.51–0.51] | 1 [1–1] | 20.40 [20.40–20.40] | 1820.5 [1820.5–1820.5] | 2090.1 [2090.1–2090.1] |         0 [0–0] | 3037.8 [3037.8–3037.8] | —             |

1000 monitors: candidate B, scheduler realm, chaos none, store json:

|   n |             startup ms | idle RSS MB |    RSS median MB |         CPU ms/poll |          CPU % core |       poll ratio |    polls/monitor |  behind |         events/s | p95 latency ms | p99 latency ms | health failures |      drain ms | widest spread |
| --: | ---------------------: | ----------: | ---------------: | ------------------: | ------------------: | ---------------: | ---------------: | ------: | ---------------: | -------------: | -------------: | --------------: | ------------: | ------------- |
|   1 | 6173.0 [6173.0–6173.0] |           — | 42.2 [42.2–42.2] | 76.59 [76.59–76.59] | 101.9 [101.9–101.9] | 0.20 [0.20–0.20] | 0.40 [0.40–0.40] | 1 [1–1] | 0.00 [0.00–0.00] |              — |              — |         0 [0–0] | 6.7 [6.7–6.7] | —             |

1000 monitors: candidate B, scheduler seq, chaos none, store json:

|   n |             startup ms | idle RSS MB |    RSS median MB |         CPU ms/poll |          CPU % core |       poll ratio |    polls/monitor |  behind |         events/s | p95 latency ms | p99 latency ms | health failures |      drain ms | widest spread |
| --: | ---------------------: | ----------: | ---------------: | ------------------: | ------------------: | ---------------: | ---------------: | ------: | ---------------: | -------------: | -------------: | --------------: | ------------: | ------------- |
|   1 | 6180.6 [6180.6–6180.6] |           — | 33.8 [33.8–33.8] | 76.81 [76.81–76.81] | 101.9 [101.9–101.9] | 0.20 [0.20–0.20] | 0.40 [0.40–0.40] | 1 [1–1] | 0.00 [0.00–0.00] |              — |              — |         0 [0–0] | 9.8 [9.8–9.8] | —             |

1000 monitors: candidate A, chaos none, store json:

|   n |             startup ms | idle RSS MB |       RSS median MB |            CPU ms/poll |          CPU % core |       poll ratio |    polls/monitor |  behind |            events/s |         p95 latency ms |         p99 latency ms | health failures |               drain ms | widest spread |
| --: | ---------------------: | ----------: | ------------------: | ---------------------: | ------------------: | ---------------: | ---------------: | ------: | ------------------: | ---------------------: | ---------------------: | --------------: | ---------------------: | ------------- |
|   1 | 5114.4 [5114.4–5114.4] |           — | 347.2 [347.2–347.2] | 177.72 [177.72–177.72] | 132.7 [132.7–132.7] | 0.11 [0.11–0.11] | 0.22 [0.22–0.22] | 1 [1–1] | 11.77 [11.77–11.77] | 1351.0 [1351.0–1351.0] | 1972.1 [1972.1–1972.1] |         0 [0–0] | 3042.9 [3042.9–3042.9] | —             |

### Package and port cost

Candidate A is the sourcefed tree as published — a 6.07 MB npm tarball and 6.45 MB of built `dist` — plus the Node 26 runtime it needs: Homebrew's `node` 26.13.1 cell is 77 MB, of which `libnode` alone is 61 MB. Candidate B is one stripped binary of 3,046,432 bytes (3.05 MB) plus the two plugin bundles it loads, 68 KB in all.

What a port costs is the machinery, not the providers. The Rust stall is 3,156 lines of Rust source, 199 of them the realm prelude's JavaScript, plus 1,083 lines of Rust tests. What it replaces, counted as non-blank lines without comment-only lines, is `packages/core` 1,216 and `packages/daemon` 1,129 — 2,345 lines of machinery — while the providers' 3,005 lines do not need porting at all: the spike runs the pinned provider code unchanged as QuickJS plugins. Candidate A's own TypeScript glue for the spike is 549 lines.

### Verdict

- Ready to serve: 2–3 ms for candidate B against 66 ms at 10 monitors, 98 ms at 100 and 92 ms idle for candidate A — twenty to fifty times faster to answer `stall.start`, although the first poll lands on the 5 s tick either way.
- Memory: 70.4 MB idle for candidate A against 7.9 MB with one realm per type and 6.0 MB sequential, and 323.7 MB against 46.0 and 37.9 MB at 500 monitors — nine to twelve times smaller idle, seven to nine times smaller loaded.
- CPU per poll: candidate B costs 2.4 to 3.4 times less with sourcefed's JSON store in place, 20 times less at 100 monitors and about 57 times less at 500 with the store out of the way.
- Capacity: with the store out of the way at 500 monitors, candidate B keeps every poll at 3.4% of a core and a p95 of 85.5 ms, where candidate A manages 0.55 polls a monitor at 107.9% of a core and a p95 of 1.23 s.
- Deal-breaker: one plugin that loops synchronously. Candidate A delivers nothing, stops answering health probes at 11.28 s and is killed at its 10 s drain timeout; candidate B answers all 33 probes, keeps the other 20 monitors' events at 1.33 a second and drains in 2–3 ms.

That last line is about the isolation model, not about Node. A Node stall could hold the same bound by putting each plugin in a worker thread, but candidate A as sourcefed specifies it today has none, so one synchronous loop in one provider takes the whole stall with it. A Rust stall with one QuickJS realm per source type gets the bound from its structure: the poll deadline interrupts the realm, the realm is rebuilt, and the rest of the stall keeps polling.

## Caveats

- The GitHub rate limiter is lifted in candidate A's vendored copy (see the deviations above), so no figure here reflects sourcefed's shipped ceiling of 30 requests a minute, or the budget accounting that will replace it.
- Credentials are placeholders: the providers' auth path is exercised, secret handling is not, because barn owns it.
- The mock upstream is a loopback stand-in. There is no TLS, no real API latency, no upstream throttling and no rate-limit backoff, and every payload is 100 issues of the same shape. A real GitHub or Jira answer is slower and more variable, which would lower every events/s figure and raise every latency figure.
- The 15 s interval stands in for sourcefed's 60 s default, so the real load is four times lighter than what is measured here. That is why every figure is given per poll and per second: those survive the change of interval, the totals do not.
- barn's own surface is not measured at all. Its sandbox, listener handover, secret handling and budgets are what this spike assumes; a stall's cost under them is still unknown, and none of these runs had a supervisor in the loop.
- Candidate A re-lists its monitors for every event delivery. That is a real cost of today's daemon that a port would drop, so candidate A's CPU per poll is, if anything, generous to candidate A.
- Candidate B's store deliberately mirrors sourcefed's whole-file store, field for field, including the re-read. That is what makes its 500-monitor tail latency 20 s, and why the memory-store tables are the fair reading of the runtime alone: with the store gone, B's p95 at 500 monitors is 85.5 ms.
- The two Rust scheduler modes are not settled here. At these sizes the store dominates, and the two land within noise of each other on polls, CPU and memory; one realm per type is slower to deliver (85.5 ms against 37.0 ms p95 at 500 monitors with the memory store), a latency difference rather than a capacity one.
- The ramp is one run per size. It is a threshold probe that finds where polls start falling behind, not a like-for-like comparison.
- Each scenario, and each ramp size, ran in its own job on its own runner, so sizes are not compared across machines. Each table names the CPU of its job: EPYC 9V45 at 10 monitors, EPYC 9V74 at 100 and for the memory runs, EPYC 7763 at 500, idle, faults and the ramp.
- Run-to-run noise is in every table's `widest spread` column. The widest cells are p99 latency on the 10-monitor steady runs, 184–234% of the median, because a handful of events at 10 monitors is too few for a p99. The 5-run tables are noisier than the 10-run ones (78.8% for p95 latency on the sequential 500-monitor memory runs, 139.2% on the sequential throwing-plugin runs), and the idle CPU cell reads 200% of a 10 ms median over a 0–20 ms range, which is a near-zero median rather than a real swing. Every other cell in the 10-run steady tables is under 50%.
- Delivery is at-least-once: an event re-delivered after an SSE reconnect is counted again, which is why candidate A's events/s can exceed the events its polls actually saw.
- Linux only: process-tree samples come from `/proc`, CPU ticks are assumed at 100 Hz, and a run cut short by the health probe still divides by its configured window.

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
