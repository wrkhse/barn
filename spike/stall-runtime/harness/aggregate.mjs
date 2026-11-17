// Turns a directory of run JSONs into the markdown the supervisor pastes into the PR, plus
// the same numbers as results/summary.json. Nothing here re-runs a measurement; every cell
// is rebuilt from the run files.
import { mkdir, readdir, readFile, writeFile } from "node:fs/promises"
import path from "node:path"
import { pathToFileURL } from "node:url"
import { median } from "./proc.mjs"

const METRICS = [
  { key: "startupMs", label: "startup ms", digits: 1 },
  { key: "idleRssMb", label: "idle RSS MB", digits: 1 },
  { key: "rssMb.median", label: "RSS median MB", digits: 1 },
  { key: "cpuMsPerPoll", label: "CPU ms/poll", digits: 2 },
  { key: "cpuPctOfOneCore", label: "CPU % core", digits: 1 },
  { key: "pollRatio", label: "poll ratio", digits: 2 },
  { key: "pollsPerMonitor", label: "polls/monitor", digits: 2 },
  { key: "behind", label: "behind", digits: 0, kind: "boolean" },
  { key: "eventsPerSec", label: "events/s", digits: 2 },
  { key: "latencyMs.p95", label: "p95 latency ms", digits: 1 },
  { key: "latencyMs.p99", label: "p99 latency ms", digits: 1 },
  { key: "health.failures", label: "health failures", digits: 0 },
  { key: "drainMs", label: "drain ms", digits: 1 },
]
const SCENARIO_ORDER = { steady: 0, quiet: 1, idle: 2, fault: 3 }
const CAVEATS = [
  "Linux only: process-tree samples come from /proc, and CPU ticks are assumed at 100 Hz.",
  "All timings use the harness's hrtime clock; no clock or counter inside either candidate is read.",
  "latencyMs is measured from the broker finishing the upstream reply to the SSE frame reaching the fake host; e2eMs (not tabulated) adds the stall's request path and the broker's upstream call.",
  "At-least-once delivery: a re-delivered event after one SSE reconnect is counted again; acks are batched every 2 s.",
  "The mock upstream is a stand-in: changing mode returns 100 issues and a fresh ETag per poll, stable mode answers 304 after priming.",
  "cpuPctOfOneCore can exceed 100% when the tree uses several cores; a run ended early by the health probe still divides by the configured window.",
  "idleRssMb is null for steady, quiet and fault runs; the idle scenario reports the window median. startupMs is spawn to first net.request, with no settle.",
  "Health probes are stall protocol RPCs every second; their CPU is part of the sampled process tree.",
  "A metric with no sample in a run is null and is left out of its cell; if no run has a sample the cell is —.",
  "The fault scenario adds a chaos monitor, so its monitor count is one higher than steady or quiet at the same setting.",
  "Run JSONs are the only record: state dirs and upstream ports are ephemeral.",
]

async function main() {
  const options = parseArgs(process.argv.slice(2))
  const runs = await loadRuns(options.dir)
  const groups = groupRuns(runs)
  const markdown = render(runs, groups)
  const summaryPath = path.join(path.dirname(options.out), "summary.json")
  const summary = {
    schema: "stall-spike/summary@1",
    generatedAt: new Date().toISOString(),
    runs: runs.length,
    machine: runs[0].machine ?? null,
    groups,
    caveats: CAVEATS,
  }
  await mkdir(path.dirname(options.out), { recursive: true })
  await writeFile(options.out, markdown)
  await writeFile(summaryPath, `${JSON.stringify(summary, null, 2)}\n`)
  process.stdout.write(`wrote ${options.out} and ${summaryPath} (${runs.length} runs, ${groups.length} groups)\n`)
}

async function loadRuns(directory) {
  const names = (await readdir(directory)).filter((name) => name.endsWith(".json") && name !== "summary.json").sort()
  const runs = []
  for (const name of names) {
    const text = await readFile(path.join(directory, name), "utf8")
    let value
    try {
      value = JSON.parse(text)
    } catch (error) {
      throw new Error(`${name}: invalid JSON: ${error.message}`)
    }
    if (value?.schema !== "stall-spike/run@1") {
      process.stderr.write(`aggregate: skipping ${name} (schema ${value?.schema ?? "missing"})\n`)
      continue
    }
    runs.push(value)
  }
  if (runs.length === 0) throw new Error(`no run files with schema stall-spike/run@1 in ${directory}`)
  return runs
}

function groupRuns(runs) {
  const byKey = new Map()
  for (const run of runs) {
    const key = [run.scenario, run.monitors, run.candidate, run.scheduler ?? "", run.chaos ?? "none", run.store ?? "json"].join("\u0000")
    const list = byKey.get(key)
    if (list) list.push(run)
    else byKey.set(key, [run])
  }
  return [...byKey.values()].map(summariseGroup).sort(compareGroups)
}

function summariseGroup(list) {
  const first = list[0]
  const metrics = {}
  for (const metric of METRICS) {
    const values = list.map((run) => metricValue(run, metric)).filter((value) => value !== null)
    metrics[metric.key] = { median: median(values), min: minimum(values), max: maximum(values), n: values.length }
  }
  return {
    scenario: first.scenario,
    monitors: first.monitors,
    candidate: first.candidate,
    scheduler: first.scheduler ?? null,
    chaos: first.chaos ?? "none",
    store: first.store ?? "json",
    n: list.length,
    metrics,
    spread: widestSpread(metrics),
  }
}

function compareGroups(left, right) {
  return (SCENARIO_ORDER[left.scenario] ?? 9) - (SCENARIO_ORDER[right.scenario] ?? 9)
    || left.monitors - right.monitors
    || left.candidate.localeCompare(right.candidate)
    || String(left.scheduler ?? "").localeCompare(String(right.scheduler ?? ""))
    || left.chaos.localeCompare(right.chaos)
    || left.store.localeCompare(right.store)
}

function widestSpread(metrics) {
  let best = null
  for (const metric of METRICS) {
    if (metric.kind === "boolean") continue
    const entry = metrics[metric.key]
    if (!entry || entry.n < 2 || entry.median === null || entry.median === 0) continue
    const pct = ((entry.max - entry.min) / Math.abs(entry.median)) * 100
    if (!best || pct > best.pct) best = { metric: metric.key, label: metric.label, pct }
  }
  return best
}

function render(runs, groups) {
  const lines = ["# Stall spike results", "", methodParagraph(runs), "", machineParagraph(runs), ""]
  for (const group of groups) {
    lines.push(groupHeading(group), "", tableHeader(), tableRule(), tableRow(group), "")
  }
  lines.push("## Caveats", "")
  for (const caveat of CAVEATS) lines.push(`- ${caveat}`)
  return `${lines.join("\n")}\n`
}

function groupHeading(group) {
  return `## ${group.scenario} · ${group.monitors} monitors · ${group.candidate} · scheduler ${group.scheduler ?? "—"} · chaos ${group.chaos} · store ${group.store}`
}

function methodParagraph(runs) {
  return `Every run spawns one stall, drives the same workload through its stall protocol and HTTP API, and samples the process tree once a second. Values are medians of the runs found (the planned 10 per cell; n says how many), with [min–max] across those runs. Request and SSE frame times come only from the harness's own hrtime clock, so the latency columns measure host-side processing and delivery after the mock upstream answered, never a clock inside either candidate. Each table heading names the scenario, monitor count, candidate, scheduler, chaos and store it covers. With priming outside the window, a candidate that kept up polls each monitor floor(windowSec/intervalSec) - 1 to floor(windowSec/intervalSec) times, so its poll ratio sits between pollRatioFloor and 1; polls/monitor is the mean across monitors, and behind is 1 for a run below that floor (a group's behind cell is the median of those 0/1 values, so 0 [0–1] means some runs fell below and some did not). ${runs.length} runs in this table.`
}

function machineParagraph(runs) {
  const machine = runs[0].machine
  if (!machine) return "Machine details were not recorded in the run files."
  const rustc = machine.rustc ? `, ${machine.rustc}` : ""
  return `Measured on GitHub Actions ubuntu-latest: ${machine.cpuModel}, ${machine.cpus} CPUs, kernel ${machine.kernel}, Node ${machine.node}${rustc}.`
}

function tableHeader() {
  const labels = ["n", ...METRICS.map((metric) => metric.label), "widest spread"]
  return `| ${labels.join(" | ")} |`
}

function tableRule() {
  return `| ${["---:", ...METRICS.map(() => "---:"), "---"].join(" | ")} |`
}

function tableRow(group) {
  const cells = [String(group.n)]
  for (const metric of METRICS) cells.push(formatCell(metric, group.metrics[metric.key]))
  cells.push(group.spread ? `${group.spread.label} ${group.spread.pct.toFixed(1)}%` : "—")
  return `| ${cells.join(" | ")} |`
}

function formatCell(metric, entry) {
  if (!entry || entry.n === 0 || entry.median === null) return "—"
  return `${entry.median.toFixed(metric.digits)} [${entry.min.toFixed(metric.digits)}–${entry.max.toFixed(metric.digits)}]`
}

function metricValue(run, metric) {
  const value = read(run, metric.key)
  if (metric.kind === "boolean") return typeof value === "boolean" ? (value ? 1 : 0) : null
  return numeric(value)
}

function read(value, key) {
  return key.split(".").reduce((current, part) => (current === null || current === undefined ? null : current[part]), value)
}

function numeric(value) {
  return typeof value === "number" && Number.isFinite(value) ? value : null
}

function minimum(values) {
  if (values.length === 0) return null
  return values.reduce((lowest, value) => (value < lowest ? value : lowest), values[0])
}

function maximum(values) {
  if (values.length === 0) return null
  return values.reduce((highest, value) => (value > highest ? value : highest), values[0])
}

function parseArgs(argv) {
  const options = { dir: null, out: null }
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index]
    const value = argv[index + 1]
    if (arg === "--dir") { options.dir = value; index += 1 }
    else if (arg === "--out") { options.out = value; index += 1 }
    else throw new Error(`unknown argument ${arg}`)
  }
  if (!options.dir) throw new Error("--dir is required")
  if (!options.out) throw new Error("--out is required")
  return options
}

if (process.argv[1] && pathToFileURL(process.argv[1]).href === import.meta.url) {
  main().catch((error) => {
    process.stderr.write(`aggregate: ${error.message}\n`)
    process.exitCode = 1
  })
}
