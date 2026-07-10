#!/usr/bin/env node

import assert from "node:assert/strict"
import { spawnSync } from "node:child_process"
import fs from "node:fs"
import os from "node:os"
import path from "node:path"

const testRoot = process.env.RIFT_PACKED_TEST_ROOT ?? os.tmpdir()
fs.mkdirSync(testRoot, { recursive: true })
const temporary = fs.mkdtempSync(path.join(testRoot, "rift-packed-unavailable-cli-"))
const launcher = path.join(process.cwd(), "node_modules", "@jparklev", "rift", "bin", "rift.js")
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
  return result
}

function assertCowUnavailable(result) {
  assert.notEqual(result.status, 0)
  assert.match(result.stderr, /copy-on-write cloning unavailable/i)
}

try {
  const source = path.join(temporary, "source")
  fs.mkdirSync(environment.HOME)
  fs.mkdirSync(source)
  fs.writeFileSync(path.join(source, "kept.txt"), "from packed unavailable CLI")

  const init = run(["init", source, "--here"])
  if (init.status !== 0) {
    assertCowUnavailable(init)
  } else {
    const status = run(["status", source, "--json"])
    assert.equal(status.status, 0, status.stderr)
    assertCowUnavailable(run(["create", source, "--name", "unavailable"]))
  }
  console.log("packed CLI rejects unavailable copy-on-write storage")
} finally {
  fs.rmSync(temporary, { recursive: true, force: true })
}
