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
  .filter((entry) => !entry.endsWith("/"))
  .sort()

const expected = [
  "package/bin/rift.js",
  "package/bun/index.js",
  "package/index.d.ts",
  "package/native.js",
  "package/node/index.js",
  "package/package.json",
]

assert.deepEqual(contents, expected, "the public package must contain only portable JavaScript")
assert(!contents.some((entry) => entry.startsWith("package/prebuilds/")), "the public package must not bundle native prebuilds")

const manifest = JSON.parse(execFileSync("tar", ["-xOf", tarball, "package/package.json"], { encoding: "utf8" }))
const tuples = [
  "darwin-arm64",
  "darwin-x64",
  "linux-arm64",
  "linux-x64",
  "windows-arm64",
  "windows-x64",
]
const packageName = "@jparklev/rift"
assert.equal(manifest.name, packageName)
assert.deepEqual(
  manifest.optionalDependencies,
  Object.fromEntries(tuples.map((tuple) => [`${packageName}-${tuple}`, manifest.version])),
  "the public package must pin every matching native package to its exact version",
)
assert.equal(manifest.publishConfig?.access, "public", "the public package must publish publicly")
for (const lifecycle of ["preinstall", "install", "postinstall", "prepare"]) {
  assert.equal(manifest.scripts?.[lifecycle], undefined, `the public package must not use ${lifecycle}`)
}

console.log(`verified portable public package in ${tarball}`)
