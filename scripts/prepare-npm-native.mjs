#!/usr/bin/env node

import fs from "node:fs"
import path from "node:path"

const [packageDirectory, executable, library] = process.argv.slice(2)
if (!packageDirectory || !executable || !library || process.argv.length !== 5) {
  console.error("usage: node scripts/prepare-npm-native.mjs <native-package-directory> <executable> <library>")
  process.exit(1)
}

const manifestPath = path.join(packageDirectory, "package.json")
const manifest = JSON.parse(fs.readFileSync(manifestPath, "utf8"))
const binaryDestination = nativeAssetPath(manifest.rift?.binary, "binary")
const libraryDestination = nativeAssetPath(manifest.rift?.library, "library")

copy(executable, binaryDestination)
copy(library, libraryDestination)
if (process.platform !== "win32") fs.chmodSync(binaryDestination, 0o755)
console.log(`prepared ${manifest.name}`)

function nativeAssetPath(relativePath, kind) {
  if (typeof relativePath !== "string" || relativePath.length === 0 || path.isAbsolute(relativePath)) {
    throw new Error(`${manifest.name} has no valid ${kind} path in package.json`)
  }
  const destination = path.resolve(packageDirectory, relativePath)
  const relative = path.relative(packageDirectory, destination)
  if (relative === "" || relative.startsWith(`..${path.sep}`) || path.isAbsolute(relative)) {
    throw new Error(`${manifest.name} has an unsafe ${kind} path in package.json`)
  }
  return destination
}

function copy(source, destination) {
  if (!fs.statSync(source).isFile()) throw new Error(`native source is not a file: ${source}`)
  fs.mkdirSync(path.dirname(destination), { recursive: true })
  fs.copyFileSync(source, destination)
}
