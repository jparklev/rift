#!/usr/bin/env node

import fs from "node:fs"

const resultPath = process.argv[2]
if (!resultPath || process.argv.length !== 3) {
  throw new Error("usage: node scripts/npm-pack-filename.mjs <npm-pack-json>")
}

const parsed = JSON.parse(fs.readFileSync(resultPath, "utf8"))
const entries = Array.isArray(parsed) ? parsed : Object.values(parsed)
const [entry] = entries
if (entries.length !== 1 || typeof entry?.filename !== "string" || entry.filename.length === 0) {
  throw new Error(`npm pack output at ${resultPath} does not describe exactly one tarball`)
}
process.stdout.write(entry.filename)
