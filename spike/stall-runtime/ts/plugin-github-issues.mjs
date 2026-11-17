// The bundle replays the pinned provider rather than a copy of it: the realm's host HTTP is
// adapted to the request function pollGithubIssues already takes, and its {cursor, events}
// pass straight through as SourceEvents.
import { pollGithubIssues } from "#pinned/github-issues"

function request(url, headers) {
  return globalThis.__stall.http(url, headers)
}

globalThis.__stall.pluginPoll = async (input) => {
  const result = await pollGithubIssues(input.source, input.cursor, request)
  return { cursor: result.cursor, events: result.events }
}
