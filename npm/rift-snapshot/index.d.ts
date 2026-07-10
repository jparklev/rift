export interface Options {
  database?: string
}

export interface AtOptions extends Options {
  at?: string
}

export interface CreateOptions extends Options {
  from?: string
  name?: string
  into?: string
  copyAll?: boolean
  hooks?: boolean
}

export interface RemoveOptions extends AtOptions {
  all?: boolean
}

export interface OfOptions extends Options {
  of?: string
}

export interface Workspace {
  path: string
  id: string
  parent: string | null
}

export type RiftErrorCode =
  | "io"
  | "database"
  | "walk"
  | "invalid_path"
  | "cow_unavailable"
  | "initialization_required"
  | "workspace_not_initialized"
  | "missing_marker"
  | "unsafe_marker"
  | "unsupported_entry"
  | "unsafe_git"
  | "not_managed"
  | "marker_mismatch"
  | "unknown_marker"
  | "already_exists"
  | "missing_rift"
  | "dangling_parent"
  | "inside_source"
  | "invalid_config"
  | "hook_failed"
  | "invalid_request"
  | "panic"
  | "serialization"

export class RiftError extends Error {
  code: RiftErrorCode
  path?: string
  constructor(input: { code: RiftErrorCode; message: string; path?: string })
}

export function init(options?: AtOptions): null
export function create(options?: CreateOptions): string
export function remove(options?: RemoveOptions & { all: true }): string[]
export function remove(options?: RemoveOptions): void
export function list(options?: OfOptions): string[]
export function ancestors(options?: OfOptions): string[]
export function status(options?: OfOptions): Workspace
export function gc(options?: Options): string[]
