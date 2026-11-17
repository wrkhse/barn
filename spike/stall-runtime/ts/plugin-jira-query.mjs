// The pinned provider's own request function builds the absolute URL from the Jira base at
// call time; the realm prelude exposes the same env var, so the adapter reads it the same
// way instead of baking a workspace URL into the bundle.
import { pollJiraQuery } from "#pinned/jira-query"

function request(pathname) {
  const base = String(process.env.SOURCEFED_JIRA_BASE_URL ?? "").replace(/\/+$/, "")
  if (!base) throw new Error("SOURCEFED_JIRA_BASE_URL is required for jira-query monitors")
  return globalThis.__stall.http(`${base}${pathname}`, { accept: "application/json" })
}

globalThis.__stall.pluginPoll = async (input) => {
  const result = await pollJiraQuery(input.source, input.cursor, request)
  return { cursor: result.cursor, events: result.events }
}
