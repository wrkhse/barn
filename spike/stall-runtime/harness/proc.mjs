// Process-tree measurement from /proc. The spike's measured runs happen on Linux (GitHub
// Actions); on any other kernel every sampling call fails with the reason, so a macOS run
// cannot silently report zeros.
import { spawnSync } from "node:child_process"
import { readdirSync, readFileSync } from "node:fs"
import os from "node:os"

// Linux reports user and system CPU time in USER_HZ ticks. The kernel's default, and the
// value assumed here, is 100 ticks per second; the harness does not change it.
const CLOCK_TICKS_PER_SECOND = 100
const MAX_TREE_PIDS = 50_000

export function sampleTree(rootPid) {
  if (!Number.isInteger(rootPid) || rootPid <= 0) throw new Error(`sampleTree: rootPid must be a positive integer, got ${rootPid}`)

  const stats = new Map()
  for (const pid of processIDs()) {
    const stat = readStat(pid)
    if (stat) stats.set(pid, stat)
  }

  const children = new Map()
  for (const [pid, stat] of stats) {
    const list = children.get(stat.ppid)
    if (list) list.push(pid)
    else children.set(stat.ppid, [pid])
  }

  const pids = []
  const seen = new Set()
  const queue = [rootPid]
  while (queue.length > 0 && pids.length < MAX_TREE_PIDS) {
    const pid = queue.pop()
    if (!stats.has(pid) || seen.has(pid)) continue
    seen.add(pid)
    pids.push(pid)
    const list = children.get(pid)
    if (list) queue.push(...list)
  }

  let rssBytes = 0
  let cpuTicks = 0
  for (const pid of pids) {
    cpuTicks += stats.get(pid).cpuTicks
    rssBytes += readRssBytes(pid)
  }

  return {
    pids: pids.sort((left, right) => left - right),
    rssBytes,
    cpuMs: cpuTicks * (1000 / CLOCK_TICKS_PER_SECOND),
  }
}

/** Samples the tree once a second; stop() takes a final sample and returns the series. */
export function treeSampler(rootPid) {
  const samples = []
  const startedAt = process.hrtime.bigint()
  let timer = null

  const take = () => {
    const tree = sampleTree(rootPid)
    samples.push({
      tMs: Number(process.hrtime.bigint() - startedAt) / 1e6,
      rssBytes: tree.rssBytes,
      cpuMs: tree.cpuMs,
    })
  }

  return {
    start() {
      if (timer) return
      take()
      timer = setInterval(take, 1000)
      timer.unref()
    },
    stop() {
      if (timer) clearInterval(timer)
      timer = null
      take()
      return samples
    },
  }
}

/** Kernel-reported model name, with os.cpus() as the non-Linux fallback. */
export function cpuModel() {
  try {
    const line = readFileSync("/proc/cpuinfo", "utf8").split("\n").find((entry) => entry.startsWith("model name"))
    if (line) return line.slice(line.indexOf(":") + 1).trim()
  } catch {
    // /proc/cpuinfo does not exist off Linux; the fallback below still names the machine.
  }
  return os.cpus()[0]?.model ?? "unknown"
}

/** Nearest-rank percentile, so no sample is invented between two real measurements. */
export function percentile(values, p) {
  if (values.length === 0) return null
  const sorted = [...values].sort((left, right) => left - right)
  const rank = Math.ceil((p / 100) * sorted.length)
  return sorted[Math.min(sorted.length, Math.max(1, rank)) - 1]
}

export function median(values) {
  return percentile(values, 50)
}

/** A nearest-rank five-number summary for one metric; n is the sample count. */
export function percentileSummary(values) {
  return { p50: percentile(values, 50), p95: percentile(values, 95), p99: percentile(values, 99), max: maximum(values), n: values.length }
}

export function maximum(values) {
  if (values.length === 0) return null
  return values.reduce((highest, value) => (value > highest ? value : highest), values[0])
}

export function machineInfo() {
  return { cpuModel: cpuModel(), cpus: os.cpus().length, kernel: os.release(), node: process.version, rustc: rustcVersion() }
}

export function rustcVersion() {
  try {
    const result = spawnSync("rustc", ["--version"], { encoding: "utf8", timeout: 2000 })
    return result.status === 0 ? result.stdout.trim() : null
  } catch {
    return null
  }
}

/** Collects a child's stderr into a bounded tail while counting every line. */
export function stderrCollector(stream) {
  const tail = []
  let lines = 0
  let buffer = ""
  const keep = (line) => {
    tail.push(line)
    if (tail.length > 20) tail.shift()
  }
  stream.setEncoding("utf8")
  stream.on("data", (chunk) => {
    buffer += chunk
    let newline = buffer.indexOf("\n")
    while (newline !== -1) {
      keep(buffer.slice(0, newline).replace(/\r$/, ""))
      lines += 1
      buffer = buffer.slice(newline + 1)
      newline = buffer.indexOf("\n")
    }
    if (buffer.length > 4096) {
      keep(buffer)
      lines += 1
      buffer = ""
    }
  })
  stream.on("end", () => {
    if (buffer.length > 0) {
      keep(buffer)
      lines += 1
      buffer = ""
    }
  })
  return { get lines() { return lines }, tail: () => [...tail] }
}

export function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms))
}

export async function waitFor(predicate, timeoutMs) {
  const deadline = Date.now() + timeoutMs
  while (Date.now() < deadline) {
    if (predicate()) return true
    await sleep(25)
  }
  return predicate()
}

export function waitForExit(child, timeoutMs) {
  if (child.exitCode !== null || child.signalCode !== null) return Promise.resolve(true)
  return new Promise((resolve) => {
    let timer = null
    const finish = (exited) => {
      clearTimeout(timer)
      child.removeListener("exit", onExit)
      resolve(exited)
    }
    const onExit = () => finish(true)
    timer = setTimeout(() => finish(false), timeoutMs)
    child.once("exit", onExit)
  })
}

export async function stopChild(child) {
  if (child.exitCode !== null || child.signalCode !== null) return
  child.kill("SIGTERM")
  if (await waitForExit(child, 2000)) return
  child.kill("SIGKILL")
  await waitForExit(child, 2000)
}

function processIDs() {
  let entries
  try {
    entries = readdirSync("/proc")
  } catch (error) {
    throw new Error(`stall-spike measures process trees on Linux only; reading /proc failed: ${error.message}`)
  }
  return entries.filter((name) => /^[0-9]+$/.test(name)).map(Number)
}

// The comm field may contain spaces and parentheses, so fields start after the last ")".
// From there: field 3 is state, field 4 is ppid, fields 14 and 15 are utime and stime.
function readStat(pid) {
  let text
  try {
    text = readFileSync(`/proc/${pid}/stat`, "utf8")
  } catch {
    return null
  }
  const close = text.lastIndexOf(")")
  if (close < 0) return null
  const fields = text.slice(close + 2).trim().split(/\s+/)
  const ppid = Number(fields[1])
  const cpuTicks = Number(fields[11]) + Number(fields[12])
  if (!Number.isInteger(ppid) || !Number.isFinite(cpuTicks)) return null
  return { ppid, cpuTicks }
}

function readRssBytes(pid) {
  let text
  try {
    text = readFileSync(`/proc/${pid}/status`, "utf8")
  } catch {
    return 0
  }
  const line = text.split("\n").find((entry) => entry.startsWith("VmRSS:"))
  if (!line) return 0
  const kilobytes = Number(line.split(/\s+/)[1])
  return Number.isFinite(kilobytes) ? kilobytes * 1024 : 0
}
