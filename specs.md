# Rift Specs

## Requirement

`rift` must be cross-platform as far as practical. Core semantics should work across macOS, Linux, and Windows. Copy-on-write is a platform/filesystem acceleration and must not define the product model.

## API

### `create`

```ts
create(input: {
  from: AbsolutePath
  name?: string
  into?: AbsolutePath
}): AbsolutePath
```

Default behavior:

- Source is `from`.
- `name` defaults to a generated directory name.
- `into` defaults to the managed rift directory.
- Copy the whole workspace, including dirty, staged, untracked, and ignored files.
- Detach `HEAD` in the new workspace.
- Return the path of the new workspace.

If `from` is already managed by Rift, create copies that exact directory. Do not resolve back to an earlier workspace. Metadata should record the immediate source rift as its parent.

Default storage is a hidden sibling directory of the original registered workspace:

```text
/projects/app/                         original workspace
/projects/.rifts/app/task-a/           created rift
/projects/.rifts/app/task-b/           created rift
```

- Created rifts must not be stored inside the workspace being copied, because an exact copy would recursively contain existing rifts.
- If `from` is an original unregistered workspace, its sibling `.rifts/<workspace-name>/` directory becomes the default destination directory.
- If `from` is already managed, descendants use the default destination directory associated with the original workspace rather than nesting storage beside each descendant.
- If `into` is provided, use it instead of the default destination directory.
- If the original workspace is itself a filesystem mount root, its sibling default destination may not support copy-on-write with it; provide `into` on the same filesystem in that case.

### `remove`

```ts
remove(input: {
  at: AbsolutePath
}): void
```

`remove` deletes a managed rift and its full descendant subtree.

- `at` must identify a rift created by this tool; the registered source root cannot be removed.
- Resolve all descendants through `parent_id` and remove their directories deepest-first.
- Verify each existing directory's `.rift` marker before deleting it.
- Refuse removal if any descendant path is missing, because it may be a moved workspace that has not been linked yet.
- After successful filesystem removal, delete the subtree records from the database.

### `link`

```ts
link(input: {
  at: AbsolutePath
  to?: AbsolutePath
}): void
```

`link` reconnects a moved managed rift to its registry record and can change its parent.

- Read the ULID from `.rift` at `at`.
- Look up the existing rift record by ULID.
- If its recorded path is `at`, leave its location unchanged.
- If its recorded path is different and missing, update it to `at`.
- If its recorded path is different and still exists, fail because this is a duplicate identity, not a move.
- If the ULID is unknown to the database, fail; `.rift` alone does not include the ancestry needed to rebuild the record.
- If `.rift` is missing, look up `at` by its absolute path. If it matches an existing record, recreate the marker with that record's ULID.
- If `.rift` is missing and `at` does not match an existing record, fail. A moved workspace without its marker cannot be identified safely.
- If `to` is provided, set the rift's parent to the managed rift at `to`.
- Refuse `to` for an original registered workspace; only rifts created by this tool can be reparented.
- Refuse `to` if it is `at` or a descendant of `at`, because reparenting must not create a cycle.

### `children`

```ts
children(input: {
  of: AbsolutePath
}): AbsolutePath[]
```

`children` returns the direct managed rifts created from `of`.

### `ancestors`

```ts
ancestors(input: {
  of: AbsolutePath
}): AbsolutePath[]
```

`ancestors` returns the managed ancestry of `of`, ordered from its immediate parent to the root workspace.

## Metadata

Metadata is stored in a central SQLite database in the platform-appropriate user data directory.

SQLite is not overkill: multiple processes and agents may create, inspect, or remove rifts concurrently. It provides cross-platform transactions and locking without building a safe JSON registry protocol.

Start with one table:

```sql
CREATE TABLE rift (
  id TEXT PRIMARY KEY,
  parent_id TEXT REFERENCES rift(id) ON DELETE CASCADE,
  path TEXT NOT NULL UNIQUE,
  created_at INTEGER NOT NULL
);

CREATE INDEX rift_parent_id_idx ON rift(parent_id);
```

- Every managed rift has a stable generated `id`.
- `id` is a ULID generated when the workspace is first registered or created.
- `id` is stored in the central database and in a `.rift` marker file at the root of the workspace.
- `.rift` contains the rift ULID and allows a moved workspace to be rediscovered and verified against the database.
- When a managed workspace is copied, the copied `.rift` marker is replaced with the new workspace's ULID.
- The original registered workspace has `parent_id = NULL`.
- A created rift has `parent_id` set to the source rift `id`.
- `path` is its current location, not its identity.
- Provenance is a rooted tree. Descendants of any rift can be listed through recursive queries over `parent_id`.
- `remove` deletes a whole subtree, so no surviving record depends on deleted ancestry.

### Moved Rifts

If a rift is moved outside the tool, its recorded path becomes missing. The tool cannot discover an arbitrary new location without being given a path or scanning a configured directory.

When `link` is run against a directory containing `.rift`, the tool reads its ULID and reconciles the database path if the recorded path no longer exists.

If both the recorded path and the provided path exist with the same ULID, the tool must refuse automatic reconciliation because the directory was copied without assigning a new identity.

## Git Integration

Git support is an integration for directories that contain repositories; it does not define the core Rift model.

When registering or creating from a Git repository:

- Add `/.rift` to `.git/info/exclude` so the identity marker does not appear in local Git status.
- Copy the directory with its staged, unstaged, untracked, ignored, and cached state intact.
- If `HEAD` resolves to a commit, detach `HEAD` in the created destination at that same commit.
- Preserve the copied index and working tree state while detaching.
- If the repository has no commits yet, leave its unborn branch state unchanged because there is no commit to detach to.

Refuse creation from a Git repository when:

- It is a linked Git worktree whose `.git` is not an independent directory.
- A merge, rebase, cherry-pick, revert, or bisect is in progress.
- Git lock or inconsistent index state makes an exact safe copy unclear.

The tool does not create branches, commit changes, or otherwise replace normal Git commands.

## Copy Strategies

Copying is implemented behind a strategy boundary so platform-specific copy-on-write backends can be added independently.

- The production strategy on Linux uses reflink cloning.
- The production strategy on macOS uses APFS `clonefile` directory cloning.
- If no implemented copy-on-write strategy succeeds, `create` fails.
- Full byte copying is not implemented as a fallback.
- Future strategies may add Windows copy-on-write support without changing the API.

## Packaging

The project ships four interfaces backed by the same implementation and metadata model:

1. Native library containing the core API and implementation.
2. CLI package providing the `rift` executable.
3. Bun FFI package for use from Bun applications.
4. Node FFI package for use from Node.js applications.

The CLI and language bindings should remain thin and expose the same API semantics as the native library.

For CLI ergonomics, `rift create` defaults `from` to the current working directory when no source path is provided.
