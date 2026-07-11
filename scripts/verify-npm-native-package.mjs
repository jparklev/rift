#!/usr/bin/env node

import assert from "node:assert/strict"
import { execFileSync } from "node:child_process"
import { tarArgs } from "./tar-args.mjs"

const [tarball, tuple] = process.argv.slice(2)
if (!tarball || !tuple || process.argv.length !== 4) {
  throw new Error("usage: node scripts/verify-npm-native-package.mjs <package.tgz> <platform-arch>")
}

const targets = {
  "darwin-arm64": { os: "darwin", cpu: "arm64", binary: "bin/rift", library: "lib/librift_ffi.dylib" },
  "darwin-x64": { os: "darwin", cpu: "x64", binary: "bin/rift", library: "lib/librift_ffi.dylib" },
  "linux-arm64": { os: "linux", cpu: "arm64", binary: "bin/rift", library: "lib/librift_ffi.so" },
  "linux-x64": { os: "linux", cpu: "x64", binary: "bin/rift", library: "lib/librift_ffi.so" },
  "windows-arm64": { os: "win32", cpu: "arm64", binary: "bin/rift.exe", library: "lib/rift_ffi.dll" },
  "windows-x64": { os: "win32", cpu: "x64", binary: "bin/rift.exe", library: "lib/rift_ffi.dll" },
}[tuple]
if (!targets) throw new Error(`unsupported Rift native target: ${tuple}`)

const contents = execFileSync("tar", tarArgs(["-tzf", tarball]), { encoding: "utf8" })
  .split("\n")
  .filter(Boolean)
  .filter((entry) => !entry.endsWith("/"))
  .sort()
assert.deepEqual(
  contents,
  ["package/package.json", `package/${targets.binary}`, `package/${targets.library}`].sort(),
  "the native package must contain exactly one CLI and one FFI library",
)

const manifest = JSON.parse(
  execFileSync("tar", tarArgs(["-xOf", tarball, "package/package.json"]), { encoding: "utf8" }),
)
assert.equal(manifest.name, `@jparklev/rift-${tuple}`)
assert.deepEqual(manifest.os, [targets.os])
assert.deepEqual(manifest.cpu, [targets.cpu])
assert.equal(manifest.rift?.binary, targets.binary)
assert.equal(manifest.rift?.library, targets.library)
assert.equal(manifest.preferUnplugged, true)
assert.equal(manifest.publishConfig?.access, "public", "the native package must publish publicly")
for (const lifecycle of ["preinstall", "install", "postinstall", "prepare"]) {
  assert.equal(manifest.scripts?.[lifecycle], undefined, `the native package must not use ${lifecycle}`)
}

if (targets.os !== "win32") {
  const listing = execFileSync("tar", tarArgs(["-tvzf", tarball]), { encoding: "utf8" })
  const binaryLine = listing.split("\n").find((line) => line.endsWith(`package/${targets.binary}`))
  assert.match(binaryLine ?? "", /^-rwx/, "the native CLI must be executable in the tarball")
}

console.log(`verified @jparklev/rift-${tuple} in ${tarball}`)
