#!/usr/bin/env node

import assert from "node:assert/strict"
import fs from "node:fs"
import os from "node:os"
import path from "node:path"
import * as rift from "@jparklev/rift"

const testRoot = process.env.RIFT_PACKED_TEST_ROOT ?? os.tmpdir()
fs.mkdirSync(testRoot, { recursive: true })
const temporary = fs.mkdtempSync(path.join(testRoot, "rift-packed-api-"))

try {
  const source = path.join(temporary, "source")
  const database = path.join(temporary, "registry.sqlite")
  fs.mkdirSync(source)
  fs.writeFileSync(path.join(source, "kept.txt"), "from packed API")

  assert.equal(rift.init({ at: source, database }), null)

  const root = rift.status({ of: source, database })
  assert.equal(root.path, fs.realpathSync(source))
  assert.equal(root.parent, null)
  assert.match(root.id, /^[0-9A-HJKMNP-TV-Z]{26}$/)

  const child = rift.create({ from: source, name: "packed-api", database })
  assert.equal(fs.readFileSync(path.join(child, "kept.txt"), "utf8"), "from packed API")
  assert.deepEqual(rift.list({ of: source, database }), [child])
  assert.deepEqual(rift.ancestors({ of: child, database }), [fs.realpathSync(source)])

  const workspace = rift.status({ of: child, database })
  assert.equal(workspace.path, child)
  assert.equal(workspace.parent, fs.realpathSync(source))

  assert.equal(rift.remove({ at: child, database }), undefined)
  assert(Array.isArray(rift.gc({ database })))
  console.log("packed FFI API lifecycle passed")
} finally {
  fs.rmSync(temporary, { recursive: true, force: true })
}
