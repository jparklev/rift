import { dlopen, toString } from "node:ffi"
import { nativeLibrary } from "../native.js"

const libraryPath = nativeLibrary()

const { functions } = dlopen(libraryPath, {
  rift_ffi_call: { arguments: ["string"], return: "pointer" },
  rift_ffi_free: { arguments: ["pointer"], return: "void" },
})

function call(request) {
  const output = functions.rift_ffi_call(JSON.stringify(request))
  if (!output) throw new Error("Rift native library returned no response")
  let response
  try {
    response = JSON.parse(toString(output))
  } finally {
    functions.rift_ffi_free(output)
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
