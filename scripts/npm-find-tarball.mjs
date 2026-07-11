#!/usr/bin/env node

import { execFileSync } from "node:child_process"
import fs from "node:fs"
import path from "node:path"
import { tarArgs } from "./tar-args.mjs"

const [directory, expectedName, expectedVersion] = process.argv.slice(2)
if (!directory || !expectedName || !expectedVersion || process.argv.length !== 5) {
  throw new Error("usage: node scripts/npm-find-tarball.mjs <directory> <package-name> <version>")
}

const matches = tarballs(directory).filter((tarball) => {
  const manifest = JSON.parse(
    execFileSync("tar", tarArgs(["-xOf", tarball, "package/package.json"]), { encoding: "utf8" }),
  )
  return manifest.name === expectedName && manifest.version === expectedVersion
})
if (matches.length !== 1) {
  throw new Error(
    `expected exactly one ${expectedName}@${expectedVersion} tarball in ${directory}, found ${matches.length}`,
  )
}
process.stdout.write(matches[0])

function tarballs(directory) {
  const entries = fs.readdirSync(directory, { recursive: true, withFileTypes: true })
  return entries
    .filter((entry) => entry.isFile() && entry.name.endsWith(".tgz"))
    .map((entry) => path.join(entry.parentPath, entry.name))
}
