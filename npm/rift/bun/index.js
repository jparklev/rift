import { CString, dlopen, ptr } from "bun:ffi"
import { nativeLibrary } from "../native.js"

const libraryPath = nativeLibrary()

const { symbols } = dlopen(libraryPath, {
  rift_ffi_call: { args: ["ptr"], returns: "ptr" },
  rift_ffi_free: { args: ["ptr"], returns: "void" },
})
const encoder = new TextEncoder()

function call(request) {
  const input = encoder.encode(`${JSON.stringify(request)}\0`)
  const output = symbols.rift_ffi_call(ptr(input))
  if (!output) throw new Error("Rift native library returned no response")
  let response
  try {
    response = JSON.parse(new CString(output).toString())
  } finally {
    symbols.rift_ffi_free(output)
  }
  if (response.status === "error") throw new RiftError(response.error)
  return response.value
}

export class RiftError extends Error {
  constructor({ code, message, path }) {
    super(message)
    this.name = "RiftError"
    this.code = code
    this.path = path
  }
}

export function init({ at = process.cwd(), database } = {}) {
  return call({ command: "init", at, database })
}

export function create({ from = process.cwd(), name, into, copyAll, hooks, database } = {}) {
  return call({ command: "create", from, name, into, copyAll, hooks, database })
}

export function remove({ at = process.cwd(), all = false, database } = {}) {
  const result = call({ command: "remove", at, all, database })
  return all ? result : undefined
}

export function list({ of = process.cwd(), database } = {}) {
  return call({ command: "list", of, database })
}

export function ancestors({ of = process.cwd(), database } = {}) {
  return call({ command: "ancestors", of, database })
}

export function status({ of = process.cwd(), database } = {}) {
  return call({ command: "status", of, database })
}

export function gc({ database } = {}) {
  return call({ command: "gc", database })
}
