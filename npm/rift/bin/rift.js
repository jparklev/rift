#!/usr/bin/env node

import childProcess from "node:child_process"
import fs from "node:fs"
import { nativeBinary } from "../native.js"

let binary
try {
  binary = nativeBinary()
} catch (error) {
  console.error(error.message)
  process.exit(1)
}

if (process.platform !== "win32") {
  try {
    fs.chmodSync(binary, 0o755)
  } catch (error) {
    console.error(`Unable to make the Rift binary executable: ${error.message}`)
    process.exit(1)
  }
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
