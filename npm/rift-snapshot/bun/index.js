import { CString, dlopen, ptr } from "bun:ffi"
import fs from "node:fs"
import os from "node:os"
import path from "node:path"
import { fileURLToPath } from "node:url"

const platform = { darwin: "darwin", linux: "linux", win32: "windows" }[os.platform()]
const arch = { arm64: "arm64", x64: "x64" }[os.arch()]
if (!platform || !arch) throw new Error(`Unsupported Rift platform: ${os.platform()}-${os.arch()}`)

const root = path.dirname(path.dirname(fileURLToPath(import.meta.url)))
const libraryName = platform === "windows" ? "rift_ffi.dll" : platform === "darwin" ? "librift_ffi.dylib" : "librift_ffi.so"
const libraryPath = path.join(root, "prebuilds", `${platform}-${arch}`, libraryName)
if (!fs.existsSync(libraryPath)) throw new Error(`Unable to locate the Rift Bun library for ${platform}-${arch}. Reinstall rift-snapshot.`)

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
  if (response.status === "error") throw new Error(response.error)
  return response.value
}

export function init({ at = process.cwd(), database } = {}) {
  return call({ command: "init", at, database })
}

export function create({ from = process.cwd(), name, into, database } = {}) {
  return call({ command: "create", from, name, into, database })
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

export function gc({ database } = {}) {
  return call({ command: "gc", database })
}
