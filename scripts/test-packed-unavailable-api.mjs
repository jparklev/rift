#!/usr/bin/env node

import assert from "node:assert/strict"
import fs from "node:fs"
import os from "node:os"
import path from "node:path"
import * as rift from "@jparklev/rift"

const testRoot = process.env.RIFT_PACKED_TEST_ROOT ?? os.tmpdir()
fs.mkdirSync(testRoot, { recursive: true })
const temporary = fs.mkdtempSync(path.join(testRoot, "rift-packed-unavailable-api-"))

function assertCowUnavailable(operation) {
  assert.throws(operation, (error) => {
    assert(error instanceof rift.RiftError)
    assert.equal(error.code, "cow_unavailable")
    return true
  })
}

try {
  const source = path.join(temporary, "source")
  const database = path.join(temporary, "registry.sqlite")
  fs.mkdirSync(source)
  fs.writeFileSync(path.join(source, "kept.txt"), "from packed unavailable API")

  // Windows can register a workspace before rejecting the unsupported copy;
  // Linux on an ordinary filesystem rejects initialization itself. Both are
  // deliberate fail-closed behavior, and this target-native harness proves
  // the binding reports the typed error rather than silently copying files.
  let initializationFailed = false
  try {
    assert.equal(rift.init({ at: source, database }), null)
  } catch (error) {
    assertCowUnavailable(() => {
      throw error
    })
    initializationFailed = true
  }

  if (!initializationFailed) {
    assertCowUnavailable(() => rift.create({ from: source, name: "unavailable", database }))
  }
  console.log("packed FFI API rejects unavailable copy-on-write storage")
} finally {
  fs.rmSync(temporary, { recursive: true, force: true })
}
