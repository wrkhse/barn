// The deterministic workload both candidates are measured on. Pure functions only, so the
// shape can be checked without a stall, a socket or a mock upstream.

const GITHUB_LABELS = ["atelier:build"]
const CHAOS_REPO = "example-org/chaos"
const CHAOS_MATCH = "repos/example-org/chaos/"
const CHAOS_MODES = new Set(["none", "throw", "loop"])

export function buildMonitors({ count, chaos = "none", intervalSec = 15 }) {
  if (!Number.isInteger(count) || count < 0) throw new Error(`buildMonitors: count must be a non-negative integer, got ${count}`)
  if (!(intervalSec > 0)) throw new Error(`buildMonitors: intervalSec must be positive, got ${intervalSec}`)
  if (!CHAOS_MODES.has(chaos)) throw new Error(`buildMonitors: unknown chaos mode ${chaos}`)

  const githubCount = Math.round(count * 0.6)
  const monitors = []
  for (let i = 1; i <= githubCount; i += 1) {
    monitors.push({
      name: `repo-${i}`,
      sourceType: "github-issues",
      params: { repo: `example-org/repo-${i}`, labels: [...GITHUB_LABELS], includeExisting: false },
      pollIntervalSec: intervalSec,
    })
  }
  for (let i = 1; i <= count - githubCount; i += 1) {
    monitors.push({
      name: `PROJ${i}`,
      sourceType: "jira-query",
      params: { jql: `project = PROJ${i} ORDER BY key ASC` },
      pollIntervalSec: intervalSec,
    })
  }
  if (chaos !== "none") {
    monitors.push({
      name: "chaos",
      sourceType: "github-issues",
      params: { repo: CHAOS_REPO, labels: [...GITHUB_LABELS], includeExisting: false },
      pollIntervalSec: intervalSec,
    })
  }
  return monitors
}

export function faultsFor(chaos) {
  if (!CHAOS_MODES.has(chaos)) throw new Error(`faultsFor: unknown chaos mode ${chaos}`)
  if (chaos === "none") return []
  return [{ match: CHAOS_MATCH, mode: chaos }]
}

/**
 * Polls that become due inside the window. Each monitor's first poll happens before the
 * window when it is created, so only whole intervals that elapse inside the window count.
 */
export function duePolls({ count, windowSec, intervalSec }) {
  if (!Number.isInteger(count) || count < 0) throw new Error(`duePolls: count must be a non-negative integer, got ${count}`)
  if (!(windowSec > 0)) throw new Error(`duePolls: windowSec must be positive, got ${windowSec}`)
  if (!(intervalSec > 0)) throw new Error(`duePolls: intervalSec must be positive, got ${intervalSec}`)
  return Math.floor(windowSec / intervalSec) * count
}
