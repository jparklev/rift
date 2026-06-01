#!/usr/bin/env node

import fs from "node:fs"
import path from "node:path"

const [target, ...sources] = process.argv.slice(2)
if (!target || sources.length === 0) {
  console.error("usage: node scripts/prepare-npm-prebuild.mjs <target-directory> <file>...")
  process.exit(1)
}

fs.mkdirSync(target, { recursive: true })
for (const source of sources) {
  const destination = path.join(target, path.basename(source))
  fs.copyFileSync(source, destination)
  fs.chmodSync(destination, 0o755)
}
