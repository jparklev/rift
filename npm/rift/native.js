import fs from "node:fs"
import os from "node:os"
import path from "node:path"
import { createRequire } from "node:module"

const require = createRequire(import.meta.url)
const platform = { darwin: "darwin", linux: "linux", win32: "windows" }[os.platform()]
const arch = { arm64: "arm64", x64: "x64" }[os.arch()]

if (!platform || !arch) {
  throw new Error(`Unsupported Rift platform: ${os.platform()}-${os.arch()}`)
}

const publicPackageName = "@jparklev/rift"
const packageName = `${publicPackageName}-${platform}-${arch}`
const publicManifest = JSON.parse(fs.readFileSync(new URL("./package.json", import.meta.url), "utf8"))
const expectedVersion = publicManifest.optionalDependencies?.[packageName]
let nativePackage

function resolveNativePackage() {
  if (nativePackage) return nativePackage

  let manifestPath
  try {
    manifestPath = require.resolve(`${packageName}/package.json`)
  } catch {
    throw new Error(`Unable to locate ${packageName}. Reinstall ${publicPackageName} with optional dependencies enabled.`)
  }

  let manifest
  try {
    manifest = JSON.parse(fs.readFileSync(manifestPath, "utf8"))
  } catch (error) {
    throw new Error(`Unable to read ${packageName}'s native manifest: ${error.message}`)
  }
  if (manifest.name !== packageName || manifest.version !== expectedVersion) {
    throw new Error(`${packageName} must be version ${expectedVersion}; reinstall ${publicPackageName} to repair the native package.`)
  }

  const root = path.dirname(manifestPath)
  nativePackage = {
    binary: resolveAsset(root, manifest.rift?.binary, "binary"),
    library: resolveAsset(root, manifest.rift?.library, "library"),
  }
  return nativePackage
}

function resolveAsset(root, relativePath, kind) {
  if (typeof relativePath !== "string" || relativePath.length === 0 || path.isAbsolute(relativePath)) {
    throw new Error(`${packageName} has no valid ${kind} path in its native manifest.`)
  }
  const asset = path.resolve(root, relativePath)
  const relative = path.relative(root, asset)
  if (relative === "" || relative.startsWith(`..${path.sep}`) || path.isAbsolute(relative)) {
    throw new Error(`${packageName} has an unsafe ${kind} path in its native manifest.`)
  }
  return asset
}

function nativeAsset(kind) {
  const asset = resolveNativePackage()[kind]
  if (!fs.existsSync(asset)) {
    throw new Error(`${packageName} is missing its ${kind} at ${asset}. Reinstall ${publicPackageName}.`)
  }
  return asset
}

export function nativeBinary() {
  return nativeAsset("binary")
}

export function nativeLibrary() {
  return nativeAsset("library")
}
