// One measured run: mock upstream, one stall process, the barn stand-in, the fake host, a
// fixed workload, and a window in which every number in the run JSON is collected. The
// harness never times its own work as the stall's: request and frame stamps come from the
// same host hrtime clock, and only work that crossed a lane or HTTP boundary is counted.
import { spawn } from "node:child_process"
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises"
import http from "node:http"
import os from "node:os"
import path from "node:path"
import { pathToFileURL } from "node:url"
import { keyForSource, rpcCall, startBroker } from "./broker.mjs"
import { machineInfo, maximum, median, percentile, percentileSummary, sleep, stderrCollector, stopChild, treeSampler, waitFor, waitForExit } from "./proc.mjs"
import { startSseHost } from "./sse.mjs"
import { spawnUpstream } from "./upstream.mjs"
import { buildMonitors, duePolls, faultsFor } from "./workload.mjs"

const TARGET = { kind: "program", id: "spike" }
const BYTES_PER_MB = 1024 * 1024
const LANE_START_TIMEOUT_MS = 15_000
const FIRST_REQUEST_TIMEOUT_MS = 15_000
const CREATE_TIMEOUT_MS = 5000
const WARMUP_MS = 2000
const HEALTH_TIMEOUT_MS = 2000
const DRAIN_TIMEOUT_MS = 5000
const KILL_TIMEOUT_MS = 2000
const STALL_START_TIMEOUT_MS = 10_000

async function main() {
  const options = parseArgs(process.argv.slice(2))
  const run = await measuredRun(options)
  const text = `${JSON.stringify(run, null, 2)}\n`
  await mkdir(path.dirname(options.out), { recursive: true })
  await writeFile(options.out, text)
  process.stdout.write(text)
}

async function measuredRun(options) {
  const startedAt = new Date().toISOString()
  const stateDir = await mkdtemp(path.join(os.tmpdir(), "stall-spike-"))
  await mkdir(path.join(stateDir, "home"), { recursive: true })
  await mkdir(path.join(stateDir, "tmp"), { recursive: true })

  let upstream = null
  let child = null
  let broker = null
  let sse = null
  let health = null
  let agent = null
  let stderr = null

  try {
    const mode = options.scenario === "quiet" || options.scenario === "idle" ? "stable" : "changing"
    const faults = faultsFor(options.chaos)
    upstream = await spawnUpstream({ mode, seed: options.seed })
    const tSpawn = nowNs()
    const monitors = options.scenario === "idle" ? [] : buildMonitors({ count: options.monitors, chaos: options.chaos, intervalSec: options.interval })
    child = await startStall(options, stateDir)
    stderr = stderrCollector(child.stderr)
    child.stdout.resume()

    broker = startBroker({ child, upstreamBase: `http://127.0.0.1:${upstream.port}`, faults })
    agent = new http.Agent({ keepAlive: true, maxSockets: 32 })

    const laneStarted = nowNs()
    let lane
    try {
      lane = await broker.startLane({ state: stateDir, faults }, LANE_START_TIMEOUT_MS)
    } catch (error) {
      throw new Error(describe(error, stderr))
    }
    const laneStartMs = msSince(laneStarted)
    if (!lane.http || !Number.isInteger(lane.http.port)) throw new Error(`lane.start returned no HTTP port: ${JSON.stringify(lane)}`)

    const sseStarted = nowNs()
    const frames = []
    sse = startSseHost({ port: lane.http.port, target: TARGET, onFrame: (frame) => { if (frame.type === "event") frames.push(frame) } })
    try {
      await sse.ready
    } catch (error) {
      throw new Error(describe(error, stderr))
    }
    const sseReadyMs = msSince(sseStarted)

    let firstRequestNs = null
    broker.onNetRequest(({ tNs }) => {
      if (firstRequestNs === null) firstRequestNs = tNs
    })

    const createdStarted = nowNs()
    if (options.scenario !== "idle") {
      for (const monitor of monitors) {
        try {
          await rpcCall({ port: lane.http.port, method: "monitor.create", params: createParams(monitor), timeoutMs: CREATE_TIMEOUT_MS, agent })
        } catch (error) {
          throw new Error(describe(new Error(`monitor.create ${monitor.name} failed: ${error.message}`), stderr))
        }
      }
    }
    const monitorsCreatedMs = msSince(createdStarted)

    let startupMs = null
    if (options.scenario !== "idle") {
      await waitFor(() => firstRequestNs !== null, FIRST_REQUEST_TIMEOUT_MS)
      if (firstRequestNs !== null) startupMs = Number(firstRequestNs - tSpawn) / 1e6
    }

    health = startHealthProbes(broker, tSpawn)
    await sleepUnless(health.state, WARMUP_MS)

    const windowStarted = nowNs()
    const sampler = treeSampler(child.pid)
    sampler.start()
    await sleepUnless(health.state, options.windowSec * 1000)
    const windowEnded = nowNs()
    const samples = sampler.stop()
    health.stop()
    await sse.stop()

    const due = duePolls({ count: monitors.length, windowSec: options.windowSec, intervalSec: options.interval })
    const metrics = computeMetrics({ samples, requestLog: broker.requestLog(), frames, windowStarted, windowEnded, windowSec: options.windowSec, idleRssMb: null, due })
    if (options.scenario === "idle") metrics.idleRssMb = metrics.rssMb.median

    const drainStarted = nowNs()
    let drainFailure = null
    try {
      const reply = await broker.drain(DRAIN_TIMEOUT_MS)
      if (!reply || reply.ok !== true) drainFailure = `lane.drain returned ${JSON.stringify(reply)}`
    } catch (error) {
      drainFailure = error.message
    }
    const exited = await waitForExit(child, DRAIN_TIMEOUT_MS)
    let killed = false
    if (!exited) {
      killed = true
      child.kill("SIGTERM")
      if (!(await waitForExit(child, KILL_TIMEOUT_MS))) {
        child.kill("SIGKILL")
        await waitForExit(child, KILL_TIMEOUT_MS)
      }
    }
    const drainMs = msSince(drainStarted)
    if (drainFailure) process.stderr.write(`stall-spike: lane.drain failed: ${drainFailure}\n`)

    const counters = broker.stats()
    return {
      schema: "stall-spike/run@1",
      candidate: options.candidate,
      scheduler: options.candidate === "rust" ? options.scheduler : null,
      scenario: options.scenario,
      chaos: options.chaos,
      store: options.env.STALL_STORE ?? "json",
      faults,
      monitors: monitors.length,
      intervalSec: options.interval,
      windowSec: options.windowSec,
      run: options.runIndex,
      startedAt,
      machine: machineInfo(),
      laneStartMs,
      sseReadyMs,
      monitorsCreatedMs,
      startupMs,
      ...metrics,
      health: healthSummary(health.state),
      stderrLines: stderr.lines,
      stderrTail: stderr.tail(),
      broker: { denials: counters.denials, malformedLines: counters.malformedLines },
      drainMs,
      exitCode: child.exitCode,
      killed,
    }
  } finally {
    if (health) health.stop()
    if (sse) await sse.stop().catch(() => {})
    if (broker) broker.stop()
    if (agent) agent.destroy()
    if (child && child.exitCode === null && child.signalCode === null) child.kill("SIGKILL")
    if (upstream) await stopChild(upstream.child)
    await rm(stateDir, { recursive: true, force: true })
  }
}

function computeMetrics({ samples, requestLog, frames, windowStarted, windowEnded, windowSec, idleRssMb, due }) {
  const completed = requestLog.filter((entry) => entry.status !== null && entry.tDoneNs !== null && entry.tDoneNs >= windowStarted && entry.tDoneNs <= windowEnded)
  const rss = samples.map((sample) => sample.rssBytes / BYTES_PER_MB)
  const first = samples[0]
  const last = samples[samples.length - 1]
  const cpuMs = first && last ? Math.max(0, last.cpuMs - first.cpuMs) : null
  const polls = completed.length
  const delivered = frames.filter((frame) => frame.tNs >= windowStarted && frame.tNs <= windowEnded)
  const events = delivered.reduce((total, frame) => total + frame.events.length, 0)

  return {
    idleRssMb,
    rssMb: { median: median(rss), p95: percentile(rss, 95), max: maximum(rss) },
    cpuMs,
    cpuMsPerPoll: cpuMs === null ? null : cpuMs / Math.max(polls, 1),
    cpuPctOfOneCore: cpuMs === null ? null : (cpuMs / (windowSec * 1000)) * 100,
    polls,
    duePolls: due,
    pollRatio: due > 0 ? polls / due : null,
    requests: polls,
    bytesIn: completed.reduce((total, entry) => total + entry.bytes, 0),
    events,
    eventsPerSec: events / windowSec,
    ...latencyStats(delivered, requestLog),
  }
}

function latencyStats(frames, requestLog) {
  const latency = []
  const e2e = []
  for (const frame of frames) {
    for (const event of frame.events) {
      const key = keyForSource(event && event.source)
      const request = key ? latestRequest(requestLog, key, frame.tNs) : null
      if (!request) continue
      latency.push(Number(frame.tNs - request.tDoneNs) / 1e6)
      e2e.push(Number(frame.tNs - request.tSentNs) / 1e6)
    }
  }
  return { latencyMs: percentileSummary(latency), e2eMs: percentileSummary(e2e) }
}

function latestRequest(requestLog, key, tNs) {
  for (let index = requestLog.length - 1; index >= 0; index -= 1) {
    const entry = requestLog[index]
    if (entry.monitorKey === key && entry.status !== null && entry.tDoneNs !== null && entry.tDoneNs <= tNs) return entry
  }
  return null
}

function startHealthProbes(broker, tSpawn) {
  const state = { samples: [], failures: 0, unresponsive: false, unresponsiveAfterMs: null, recoveredAfterMs: null }
  let consecutive = 0
  let stopped = false
  let timer = null

  const probe = async () => {
    if (stopped) return
    const started = nowNs()
    try {
      const reply = await broker.health(HEALTH_TIMEOUT_MS)
      if (!reply || reply.ok !== true) throw new Error(`lane.health returned ${JSON.stringify(reply)}`)
      if (stopped) return
      const now = nowNs()
      state.samples.push({ tMs: elapsed(now, tSpawn), rttMs: elapsed(now, started) })
      consecutive = 0
      if (state.unresponsive && state.recoveredAfterMs === null) state.recoveredAfterMs = elapsed(now, tSpawn)
    } catch {
      if (stopped) return
      const now = nowNs()
      consecutive += 1
      state.failures += 1
      state.samples.push({ tMs: elapsed(now, tSpawn), rttMs: null })
      if (consecutive >= 3 && !state.unresponsive) {
        state.unresponsive = true
        state.unresponsiveAfterMs = elapsed(now, tSpawn)
      }
    }
    if (stopped) return
    timer = setTimeout(probe, Math.max(0, 1000 - elapsed(nowNs(), started)))
    timer.unref()
  }

  probe().catch((error) => process.stderr.write(`stall-spike: health probe failed: ${error.message}\n`))
  return {
    state,
    stop() {
      stopped = true
      if (timer) clearTimeout(timer)
      timer = null
    },
  }
}

function healthSummary(state) {
  const rtts = state.samples.filter((sample) => sample.rttMs !== null).map((sample) => sample.rttMs)
  return {
    probes: state.samples.length,
    failures: state.failures,
    p95Ms: percentile(rtts, 95),
    unresponsive: state.unresponsive,
    unresponsiveAfterMs: state.unresponsiveAfterMs,
    recoveredAfterMs: state.recoveredAfterMs,
  }
}

function createParams(monitor) {
  return { target: TARGET, name: monitor.name, sourceType: monitor.sourceType, ...monitor.params, pollIntervalSec: monitor.pollIntervalSec }
}

function stallEnv(options, stateDir) {
  const env = {
    PATH: process.env.PATH ?? "/usr/local/bin:/usr/bin:/bin",
    HOME: path.join(stateDir, "home"),
    TMPDIR: path.join(stateDir, "tmp"),
    SOURCEFED_JIRA_BASE_URL: "https://example.atlassian.net",
    GH_TOKEN: "spike-token",
    // The mock upstream ignores auth, but sourcefed's Jira provider refuses to poll without
    // these; they only ever reach the loopback mock because every request goes over the lane.
    ATLASSIAN_EMAIL: "spike@example.com",
    ATLASSIAN_API_KEY: "spike-token",
  }
  // Candidate A's env deliberately leaves SOURCEFED_GITHUB_REQUESTS_PER_MINUTE unset: both
  // candidates lift sourcefed's GitHub limiter to 1e6 (scripts/vendor.mjs), and any lower value
  // would re-cap A alone and measure the limiter instead of the runtime. --env can still set it
  // for an explicit budget experiment.
  if (options.candidate === "rust") env.STALL_SCHEDULER = options.scheduler
  return { ...env, ...options.env }
}

function startStall(options, stateDir) {
  const command = options.node ? process.execPath : options.stallCmd
  const args = options.node ? [options.stallCmd] : []
  const child = spawn(command, args, { stdio: ["ignore", "pipe", "pipe", "pipe"], env: stallEnv(options, stateDir) })
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(`stall did not start within 10 s: ${command}`)), STALL_START_TIMEOUT_MS)
    child.once("spawn", () => {
      clearTimeout(timer)
      resolve(child)
    })
    child.once("error", (error) => {
      clearTimeout(timer)
      reject(new Error(`stall failed to start (${command}): ${error.message}`))
    })
  })
}

async function sleepUnless(state, ms) {
  const deadline = Date.now() + ms
  while (Date.now() < deadline) {
    if (state.unresponsive) return false
    await sleep(Math.min(250, deadline - Date.now()))
  }
  return !state.unresponsive
}

function describe(error, stderr) {
  return `${error.message}\nstall stderr tail:\n${stderr.tail().join("\n") || "(empty)"}`
}

function elapsed(ns, startNs) {
  return Number(ns - startNs) / 1e6
}

function msSince(startNs) {
  return Number(nowNs() - startNs) / 1e6
}

function nowNs() {
  return process.hrtime.bigint()
}

function parseArgs(argv) {
  const options = { candidate: null, scheduler: null, scenario: "steady", monitors: 100, interval: 15, windowSec: 60, chaos: "none", seed: 1, runIndex: 1, node: false, env: {}, out: null, stallCmd: null }
  const take = (index, arg) => {
    const value = argv[index + 1]
    if (value === undefined) throw new Error(`${arg} needs a value`)
    return value
  }
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index]
    const value = () => take(index++, arg)
    if (arg === "--candidate") options.candidate = value()
    else if (arg === "--scheduler") options.scheduler = value()
    else if (arg === "--scenario") options.scenario = value()
    else if (arg === "--monitors") options.monitors = Number(value())
    else if (arg === "--interval") options.interval = Number(value())
    else if (arg === "--window") options.windowSec = Number(value())
    else if (arg === "--chaos") options.chaos = value()
    else if (arg === "--seed") options.seed = Number(value())
    else if (arg === "--run-index") options.runIndex = Number(value())
    else if (arg === "--out") options.out = value()
    else if (arg === "--stall-cmd") options.stallCmd = value()
    else if (arg === "--node") options.node = true
    else if (arg === "--env") {
      const pair = value()
      const equals = pair.indexOf("=")
      if (equals < 1) throw new Error(`--env needs key=value, got ${pair}`)
      options.env[pair.slice(0, equals)] = pair.slice(equals + 1)
    } else throw new Error(`unknown argument ${arg}`)
  }

  if (options.candidate !== "ts" && options.candidate !== "rust") throw new Error("--candidate must be ts or rust")
  if (!["steady", "quiet", "idle", "fault"].includes(options.scenario)) throw new Error("--scenario must be steady, quiet, idle or fault")
  if (!["none", "throw", "loop"].includes(options.chaos)) throw new Error("--chaos must be none, throw or loop")
  if (!Number.isInteger(options.monitors) || options.monitors < 0) throw new Error("--monitors must be a non-negative integer")
  if (!(options.interval > 0)) throw new Error("--interval must be positive")
  if (!(options.windowSec > 0)) throw new Error("--window must be positive")
  if (!Number.isInteger(options.seed)) throw new Error("--seed must be an integer")
  if (!Number.isInteger(options.runIndex) || options.runIndex < 1) throw new Error("--run-index must be a positive integer")
  if (options.scenario === "fault" && options.chaos === "none") throw new Error("--scenario fault needs --chaos throw or loop")
  if (options.scenario !== "fault" && options.chaos !== "none") throw new Error("--chaos is only used with --scenario fault")
  if (options.candidate === "ts" && options.scheduler !== null) throw new Error("--scheduler is only used with --candidate rust")
  if (options.candidate === "rust" && options.scheduler === null) options.scheduler = "realm"
  if (options.scheduler !== null && !["seq", "realm"].includes(options.scheduler)) throw new Error("--scheduler must be seq or realm")
  if (!options.out) throw new Error("--out is required")
  if (!options.stallCmd) throw new Error("--stall-cmd is required")
  return options
}

if (process.argv[1] && pathToFileURL(process.argv[1]).href === import.meta.url) {
  main().catch((error) => {
    process.stderr.write(`stall-spike: ${error.message}\n`)
    process.exitCode = 1
  })
}
