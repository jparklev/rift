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
}

export interface RemoveOptions extends AtOptions {
  all?: boolean
}

export interface OfOptions extends Options {
  of?: string
}

export function init(options?: AtOptions): string | null
export function create(options?: CreateOptions): string
export function remove(options?: RemoveOptions & { all: true }): string[]
export function remove(options?: RemoveOptions): void
export function list(options?: OfOptions): string[]
export function ancestors(options?: OfOptions): string[]
export function gc(options?: Options): string[]
