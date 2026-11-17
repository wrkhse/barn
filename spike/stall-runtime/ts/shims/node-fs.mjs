// The pinned provider graph reaches @sourcefed/core's storage module, which imports
// node:fs/promises. A plugin realm has no filesystem, so every entry point throws if a poll
// path ever touches storage; importing the module itself stays harmless.

function unavailable() {
  throw new Error("filesystem access is unavailable inside a plugin realm")
}

export const link = unavailable
export const mkdir = unavailable
export const open = unavailable
export const readFile = unavailable
export const rename = unavailable
export const rm = unavailable
export const writeFile = unavailable
