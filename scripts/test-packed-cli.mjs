#!/usr/bin/env node

import assert from "node:assert/strict"
import { spawnSync } from "node:child_process"
import fs from "node:fs"
import os from "node:os"
import path from "node:path"

const temporary = fs.mkdtempSync(path.join(os.tmpdir(), "rift-packed-cli-"))
const launcher = path.join(process.cwd(), "node_modules", "rift-snapshot", "bin", "rift.js")
const dataHome = path.join(temporary, "data")
const environment = {
  ...process.env,
  HOME: path.join(temporary, "home"),
  XDG_DATA_HOME: dataHome,
  LOCALAPPDATA: dataHome,
}

function run(arguments_) {
  const result = spawnSync(process.execPath, [launcher, ...arguments_], {
    encoding: "utf8",
    env: environment,
  })
  if (result.error) throw result.error
  if (result.status !== 0) {
    throw new Error(`rift ${arguments_.join(" ")} failed:\n${result.stderr}`)
  }
  return result.stdout.trim()
}

try {
  const source = path.join(temporary, "source")
  fs.mkdirSync(environment.HOME)
  fs.mkdirSync(source)
  fs.writeFileSync(path.join(source, "kept.txt"), "from packed CLI")

  run(["init", source, "--here"])
  const status = JSON.parse(run(["status", source, "--json"]))
  assert.equal(status.path, fs.realpathSync(source))
  assert.equal(status.parent, null)

  const child = run(["create", source, "--name", "packed-cli"])
  assert.equal(fs.readFileSync(path.join(child, "kept.txt"), "utf8"), "from packed CLI")
  assert.deepEqual(run(["list", source]).split("\n").filter(Boolean), [child])

  run(["remove", child])
  run(["gc"])
  console.log("packed CLI lifecycle passed")
} finally {
  fs.rmSync(temporary, { recursive: true, force: true })
}
