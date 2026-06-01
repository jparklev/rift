# Rift

<p align="center">
  <strong>Instant workspaces for work that should not wait.</strong>
</p>

<p align="center">
  <a href="https://github.com/anomalyco/rift/actions/workflows/release.yml"><img alt="Release" src="https://img.shields.io/github/actions/workflow/status/anomalyco/rift/release.yml?label=release&style=flat-square"></a>
  <a href="https://github.com/anomalyco/rift/releases/latest"><img alt="GitHub Release" src="https://img.shields.io/github/v/release/anomalyco/rift?style=flat-square"></a>
  <a href="https://www.npmjs.com/package/rift-snapshot"><img alt="npm" src="https://img.shields.io/npm/v/rift-snapshot?style=flat-square"></a>
  <a href="./Cargo.toml"><img alt="MIT" src="https://img.shields.io/badge/license-MIT-111?style=flat-square"></a>
</p>

Rift creates full, copy-on-write workspace forks in an instant. Not clean checkouts. Not partial Git state. The directory you have right now: staged files, dirty files, ignored files, build state, everything.

Use it when you want another agent, another experiment, or another idea running in parallel without negotiating over one working tree.

```bash
npm i -g rift-snapshot # don't worry it's written in Rust

cd ~/code/app
eval "$(rift shell-init zsh)" # optional: make Rift navigate for you
rift init                   # once on Linux/btrfs
rift create                 # creates and enters the workspace
```

`rift-snapshot` is the temporary npm package name while `rift` is being acquired. The command is already `rift`.

## Why Rift

- Full workspace snapshots, including uncommitted and ignored state.
- Copy-on-write creation instead of walking and recopying large trees.
- Real lineage: create a rift from another rift and keep going.
- Detached Git `HEAD` in new workspaces, without touching staged or dirty work.
- Immediate removal from active use, with physical cleanup deferred to `rift gc`.
- One npm package with a native CLI plus Bun and Node FFI bindings. No postinstall scripts.

## Install

```bash
npm install -g rift-snapshot
# or
bun add -g rift-snapshot
```

Native archives are also published on [GitHub Releases](https://github.com/anomalyco/rift/releases/latest).

### Shell integration

Rift cannot change its parent shell as a standalone executable. Enable its small shell wrapper once per session, or from your shell config, to make workspace commands navigate automatically:

```bash
eval "$(rift shell-init zsh)" # or bash
```

With the wrapper enabled:

```bash
rift init                      # enters the initialized subvolume after conversion
rift create --name parser-fix  # enters the created rift
rift remove                    # returns to its parent before removing the current rift
```

### Platforms

| Platform          | Creation backend         | Status                                               |
| ----------------- | ------------------------ | ---------------------------------------------------- |
| Linux x64         | Writable btrfs snapshots | Supported; run `rift init` once per source workspace |
| macOS arm64 / x64 | APFS `clonefile`         | Supported                                            |
| Windows x64       | Native package published | Copy-on-write backend not implemented yet            |

On Linux, `rift init` converts an ordinary btrfs directory into a subvolume once and retains the original as `<name>.rift-backup`. After that, `rift create` uses native btrfs snapshots.

## CLI

### Create a workspace

```bash
rift create                    # fork the current workspace
rift create --name parser-fix  # choose a readable name
rift create --into /fast/rifts # choose storage location
```

Rift prints the new workspace path, so this is convenient:

```bash
cd "$(rift create --name parser-fix)"
```

With shell integration enabled, simply run `rift create --name parser-fix` and Rift enters it.

If the source is a Git repository, the new workspace starts at a detached `HEAD` while preserving its exact index and working tree state.

### Navigate the tree

```bash
rift list                      # direct active children of the current workspace
rift ancestors                 # parent chain, nearest first
```

Creating from a rift records its immediate parent while storing workspaces side by side:

```text
~/code/app/                         original workspace
~/code/.rifts/app/parser-fix/       child rift
~/code/.rifts/app/alternate-route/  descendant or sibling rift
```

### Remove and clean up

```bash
rift remove                    # remove the current rift subtree from active use
rift remove --all ~/code/app   # remove all descendants, preserve the root
rift gc                        # physically delete removed workspaces
```

`remove` is intentionally fast: it moves rifts into adjacent `.trash/` storage and removes them from the active tree. `gc` reclaims the filesystem storage later. On standard btrfs mounts, reclamation may walk files when populated subvolume deletion is not permitted to the current user.

## JavaScript API

The npm package exposes the same native implementation to Bun and Node through conditional exports:

```js
import { create, list, remove, gc } from "rift-snapshot";

const workspace = create({ from: process.cwd(), name: "schema-work" });
console.log(list({ of: process.cwd() }));

remove({ at: workspace });
gc();
```

### Bun

```bash
bun add rift-snapshot
```

```ts
import { create } from "rift-snapshot";

const workspace = create({ from: process.cwd() });
```

The Bun binding uses `bun:ffi` and is statically analyzable for `bun build --compile`; required native libraries are embedded from the package prebuilds.

### Node.js

Node bindings use the experimental `node:ffi` API available in Node.js 26.1+:

```bash
npm install rift-snapshot
node --experimental-ffi app.mjs
```

```js
import { create } from "rift-snapshot";

const workspace = create({ from: process.cwd() });
```

If using Node's permission model, also pass `--allow-ffi`.

### API

```ts
init(options?: { at?: string; database?: string }): string | null
create(options?: { from?: string; name?: string; into?: string; database?: string }): string
remove(options?: { at?: string; all?: false; database?: string }): void
remove(options: { at?: string; all: true; database?: string }): string[]
list(options?: { of?: string; database?: string }): string[]
ancestors(options?: { of?: string; database?: string }): string[]
gc(options?: { database?: string }): string[]
```

Path inputs default to the current working directory where applicable. `database` is useful for isolated tooling and tests; normal use relies on Rift's user-local registry.

## Design Notes

Rift stores stable identities in `.rift` marker files and ancestry in a local SQLite registry. Active workspace storage lives outside the source being snapshotted, normally in a hidden sibling directory:

```text
~/code/app/
~/code/.rifts/app/
```

Rift does not create branches, commit changes, or replace Git. It gives your current filesystem state somewhere new to run.

## Development

```bash
cargo test --workspace --locked
./scripts/build.sh
```

The debug CLI is written to `target/debug/rift`.

## License

MIT
