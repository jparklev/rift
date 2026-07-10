#!/usr/bin/env node

import assert from "node:assert/strict"
import { spawnSync } from "node:child_process"
import os from "node:os"
import path from "node:path"

const platform = { darwin: "darwin", linux: "linux", win32: "windows" }[os.platform()]
const arch = { arm64: "arm64", x64: "x64" }[os.arch()]
const packageName = `@jparklev/rift-${platform}-${arch}`

try {
  await import("@jparklev/rift")
  assert.fail("the public package should fail without its optional native package")
} catch (error) {
  assert.match(String(error.message), new RegExp(packageName))
}

const launcher = path.join(process.cwd(), "node_modules", "@jparklev", "rift", "bin", "rift.js")
const cli = spawnSync(process.execPath, [launcher, "--version"], { encoding: "utf8" })
assert.notEqual(cli.status, 0, "the CLI should fail without the native package")
assert.match(`${cli.stdout}${cli.stderr}`, new RegExp(packageName))
console.log("missing native package fails clearly")
