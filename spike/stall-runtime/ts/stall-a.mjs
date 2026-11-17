import { InMemoryMonitorEventQueue, InMemoryMonitorStore } from "@sourcefed/core"
import { SourcefedDaemon, daemonHealth, handleDaemonHttpRequest, serveHttp } from "@sourcefed/daemon"
import { installBrokerFetch } from "./broker-fetch.mjs"
import { openStallProtocol } from "./stall-protocol.mjs"

// Candidate A is the real v0.5.2 daemon. Only three things differ from a stock install:
// global fetch is replaced by the broker client, the vendored copy's GitHub limiter is
// lifted, and STALL_STORE=memory swaps both stores (the json default is untouched). The
// numbers are therefore what sourcefed does today plus one broker hop.
//
// The broker is the upstream identity, so the provider gates only need to be satisfied: the
// GitHub token never reaches the network, and the Jira credentials only let the provider
// build its request. The base URL gives the Jira URL its shape.
if (!process.env.GH_TOKEN && !process.env.GITHUB_TOKEN) process.env.GITHUB_TOKEN = "spike-broker"
process.env.ATLASSIAN_EMAIL ??= "spike@example.com"
process.env.ATLASSIAN_API_KEY ??= "spike-broker"
process.env.SOURCEFED_JIRA_BASE_URL ??= "https://example.atlassian.net"

// A budget set below the lifted ceiling keeps deferring github polls, which hides the very
// runtime this candidate measures; say so on every run rather than let the numbers mislead.
const githubBudget = Number(process.env.SOURCEFED_GITHUB_REQUESTS_PER_MINUTE ?? "")
if (Number.isFinite(githubBudget) && githubBudget > 0 && githubBudget < 1_000_000) {
  console.error(`[stall-a] SOURCEFED_GITHUB_REQUESTS_PER_MINUTE=${process.env.SOURCEFED_GITHUB_REQUESTS_PER_MINUTE} re-caps the lifted GitHub limiter; set it to 1000000 to measure the runtime rather than the limiter`)
}

let daemon
let server
let brokerInflight = () => 0
let started = false
const enabledMonitors = new Set()

const protocol = openStallProtocol({
  handlers: {
    "stall.start": start,
    "stall.health": health,
    "stall.drain": drain,
    "net.request": refuseNetRequest,
  },
  onClose,
})

async function start(params) {
  if (started) throw new Error("stall.start was already handled")
  if (params.protocol !== 1) throw new Error(`unsupported stall protocol ${params.protocol}`)

  const stateDir = typeof params.state === "string" && params.state !== "" ? params.state : process.env.STALL_STATE_DIR
  if (!stateDir) throw new Error("stall.start needs state, or STALL_STATE_DIR")

  brokerInflight = installBrokerFetch({
    request: (method, requestParams) => protocol.request(method, requestParams),
    faults: params.faults ?? [],
    sendLog: (level, message) => protocol.sendLog(level, message),
  }).inflight

  daemon = openDaemon(stateDir)
  trackMonitors(daemon)
  await daemon.start()
  // A reused state dir can already hold monitors; one read here keeps the health counter
  // honest without reading the store on every probe.
  for (const monitor of await daemon.service.list()) {
    if (monitor.enabled) enabledMonitors.add(monitor.id)
  }

  const host = String(params.http?.host ?? "127.0.0.1")
  const port = Number(params.http?.port ?? 0)
  server = await serveHttp({ hostname: host, port, handler: (request) => handleDaemonHttpRequest(daemon, request) })

  started = true
  const version = daemonHealth().version
  console.error(`[stall-a] ready: sourcefed ${version}, http ${host}:${server.port}, state ${stateDir}`)
  return { ready: true, version, http: { port: server.port } }
}

async function health() {
  if (!started) throw new Error("stall.health arrived before stall.start")
  return { ok: true, monitors: enabledMonitors.size, inflight: brokerInflight() }
}

async function drain() {
  if (daemon) {
    // The daemon's own stop waits up to 30 s for in-flight ticks; the stall protocol gives it three.
    await Promise.race([daemon.stop().catch(() => undefined), delay(3_000)])
  }
  return { ok: true }
}

// STALL_STORE=memory measures the storage layer's share: the same daemon and runtime,
// with the records and the event queue held in the process. Json remains the default and
// keeps sourcefed's own construction untouched.
function openDaemon(stateDir) {
  const mode = process.env.STALL_STORE ?? "json"
  if (mode === "json") {
    console.error("[stall-a] store: json")
    return new SourcefedDaemon({ stateDir })
  }
  if (mode === "memory") {
    console.error("[stall-a] store: memory")
    return new SourcefedDaemon({ stateDir, store: new InMemoryMonitorStore(), eventQueue: new InMemoryMonitorEventQueue() })
  }
  throw new Error(`STALL_STORE must be json or memory, got ${JSON.stringify(mode)}`)
}

// The harness probes health once a second. Reading the store there would charge the stall
// for the probe (the JSON store re-parses monitors.json every time), so the count follows
// the daemon's own mutations instead.
function trackMonitors(sourcefed) {
  const createMonitor = sourcefed.createMonitor.bind(sourcefed)
  const startMonitor = sourcefed.startMonitor.bind(sourcefed)
  const stopMonitor = sourcefed.stopMonitor.bind(sourcefed)
  const removeMonitor = sourcefed.removeMonitor.bind(sourcefed)

  sourcefed.createMonitor = async (target, input) => {
    const result = await createMonitor(target, input)
    if (result.ok && result.created && result.monitor) enabledMonitors.add(result.monitor.id)
    return result
  }

  sourcefed.startMonitor = async (target, id) => {
    const result = await startMonitor(target, id)
    if (result.ok) enabledMonitors.add(String(id))
    return result
  }

  sourcefed.stopMonitor = async (target, id) => {
    const result = await stopMonitor(target, id)
    if (result.ok) enabledMonitors.delete(String(id))
    return result
  }

  sourcefed.removeMonitor = async (target, id) => {
    const result = await removeMonitor(target, id)
    if (result.ok) enabledMonitors.delete(String(id))
    return result
  }
}

// net.request is the stall's direction of the stall protocol. A harness asking the stall
// for one is a protocol mistake, and saying so is clearer than an unknown-method error.
async function refuseNetRequest() {
  throw new Error("net.request arrives from the stall, not the harness")
}

function onClose(info) {
  if (info.error) console.error(`[stall-a] stall protocol closed: ${info.error.message}`)
  // A clean end of the protocol is the harness letting go; a protocol failure is a crash.
  process.exit(info.fatal ? 1 : 0)
}

function delay(ms) {
  return new Promise((resolve) => {
    const timer = setTimeout(resolve, ms)
    timer.unref?.()
  })
}

process.on("uncaughtException", (error) => {
  console.error(`[stall-a] uncaught exception: ${error instanceof Error ? (error.stack ?? error.message) : String(error)}`)
  process.exit(1)
})

process.on("unhandledRejection", (reason) => {
  console.error(`[stall-a] unhandled rejection: ${reason instanceof Error ? (reason.stack ?? reason.message) : String(reason)}`)
  process.exit(1)
})
