#!/usr/bin/env node

import assert from "node:assert/strict"
import { execFileSync } from "node:child_process"

const tarball = process.argv[2]
if (!tarball) {
  throw new Error("usage: node scripts/verify-npm-package.mjs <package.tgz>")
}

const contents = execFileSync("tar", ["-tzf", tarball], { encoding: "utf8" })
  .split("\n")
  .filter(Boolean)
  .sort()
const prebuilds = contents.filter((entry) => entry.startsWith("package/prebuilds/"))

const expected = [
  "package/prebuilds/darwin-arm64/librift_ffi.dylib",
  "package/prebuilds/darwin-arm64/rift",
  "package/prebuilds/darwin-x64/librift_ffi.dylib",
  "package/prebuilds/darwin-x64/rift",
  "package/prebuilds/linux-arm64/librift_ffi.so",
  "package/prebuilds/linux-arm64/rift",
  "package/prebuilds/linux-x64/librift_ffi.so",
  "package/prebuilds/linux-x64/rift",
  "package/prebuilds/windows-arm64/rift.exe",
  "package/prebuilds/windows-arm64/rift_ffi.dll",
  "package/prebuilds/windows-x64/rift.exe",
  "package/prebuilds/windows-x64/rift_ffi.dll",
]

assert.deepEqual(prebuilds, expected, "the packed package must contain every declared platform tuple")
for (const required of ["package/bin/rift.js", "package/bun/index.js", "package/node/index.js"]) {
  assert(contents.includes(required), `missing ${required} from packed package`)
}

console.log(`verified ${expected.length} native files in ${tarball}`)
