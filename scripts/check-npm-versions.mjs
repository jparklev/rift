#!/usr/bin/env node

import fs from "node:fs"
import path from "node:path"

const version = process.argv[2]
if (!version) {
  console.error("usage: node scripts/check-npm-versions.mjs <version>")
  process.exit(1)
}

for (const entry of fs.readdirSync("npm")) {
  const manifest = JSON.parse(fs.readFileSync(path.join("npm", entry, "package.json"), "utf8"))
  if (manifest.version !== version) {
    throw new Error(`${manifest.name} version ${manifest.version} does not match ${version}`)
  }
  for (const [dependency, dependencyVersion] of Object.entries(manifest.optionalDependencies ?? {})) {
    if (dependencyVersion !== version) throw new Error(`${manifest.name} depends on ${dependency}@${dependencyVersion}`)
  }
}
