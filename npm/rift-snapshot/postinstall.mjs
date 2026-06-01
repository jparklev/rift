#!/usr/bin/env node

import childProcess from "node:child_process"
import fs from "node:fs"
import os from "node:os"
import path from "node:path"
import { createRequire } from "node:module"
import { fileURLToPath } from "node:url"

const directory = path.dirname(fileURLToPath(import.meta.url))
const require = createRequire(import.meta.url)
const packageJson = JSON.parse(fs.readFileSync(path.join(directory, "package.json"), "utf8"))

const platform = { darwin: "darwin", linux: "linux", win32: "windows" }[os.platform()]
const arch = { arm64: "arm64", x64: "x64" }[os.arch()]
const targetBinary = path.join(directory, "bin", "rift.exe")

function packageName() {
  if (!platform || !arch) throw new Error(`Unsupported Rift platform: ${os.platform()}-${os.arch()}`)
  const name = `rift-snapshot-${platform}-${arch}`
  if (!packageJson.optionalDependencies?.[name]) throw new Error(`Unsupported Rift platform: ${platform}-${arch}`)
  return name
}

function sourceBinary(name) {
  const manifest = require.resolve(`${name}/package.json`)
  return path.join(path.dirname(manifest), "bin", platform === "windows" ? "rift.exe" : "rift")
}

function copyBinary(source) {
  if (!fs.existsSync(source)) throw new Error(`Rift binary not found at ${source}`)
  fs.rmSync(targetBinary, { force: true })
  try {
    fs.linkSync(source, targetBinary)
  } catch {
    fs.copyFileSync(source, targetBinary)
  }
  fs.chmodSync(targetBinary, 0o755)
}

function installFallback(name) {
  const version = packageJson.optionalDependencies[name]
  const temporary = fs.mkdtempSync(path.join(os.tmpdir(), "rift-install-"))
  try {
    const result = childProcess.spawnSync(
      "npm",
      ["install", "--ignore-scripts", "--no-save", "--loglevel=error", "--prefix", temporary, `${name}@${version}`],
      { stdio: "inherit", windowsHide: true },
    )
    if (result.status !== 0) return false
    copyBinary(path.join(temporary, "node_modules", name, "bin", platform === "windows" ? "rift.exe" : "rift"))
    return true
  } finally {
    fs.rmSync(temporary, { recursive: true, force: true })
  }
}

function verifyBinary() {
  return childProcess.spawnSync(targetBinary, ["--help"], { stdio: "ignore", windowsHide: true }).status === 0
}

try {
  const name = packageName()
  try {
    copyBinary(sourceBinary(name))
  } catch {
    if (!installFallback(name)) throw new Error(`Unable to install ${name}`)
  }
  if (!verifyBinary()) throw new Error(`Installed Rift binary failed validation for ${name}`)
} catch (error) {
  console.error(error.message)
  process.exit(1)
}
