#!/usr/bin/env node

import childProcess from "node:child_process"
import os from "node:os"
import path from "node:path"
import { createRequire } from "node:module"

const require = createRequire(import.meta.url)
const platform = { darwin: "darwin", linux: "linux", win32: "windows" }[os.platform()]
const arch = { arm64: "arm64", x64: "x64" }[os.arch()]

if (!platform || !arch) {
  console.error(`Unsupported Rift platform: ${os.platform()}-${os.arch()}`)
  process.exit(1)
}

const name = `rift-snapshot-${platform}-${arch}`
let binary
try {
  const manifest = require.resolve(`${name}/package.json`)
  binary = path.join(path.dirname(manifest), "bin", platform === "windows" ? "rift.exe" : "rift")
} catch {
  console.error(`Unable to locate ${name}. Reinstall rift-snapshot with optional dependencies enabled.`)
  process.exit(1)
}

const result = childProcess.spawnSync(binary, process.argv.slice(2), {
  stdio: "inherit",
  windowsHide: true,
})
if (result.error) {
  console.error(result.error.message)
  process.exit(1)
}
process.exit(result.status ?? 1)
