#!/usr/bin/env node

import fs from "node:fs"
import path from "node:path"

const [packageDirectory, binary] = process.argv.slice(2)
if (!packageDirectory || !binary) {
  console.error("usage: node scripts/prepare-npm-package.mjs <package-directory> <binary>")
  process.exit(1)
}

const target = path.join(packageDirectory, "bin", path.basename(binary))
fs.mkdirSync(path.dirname(target), { recursive: true })
fs.copyFileSync(binary, target)
fs.chmodSync(target, 0o755)
