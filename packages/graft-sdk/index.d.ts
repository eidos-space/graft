export type SessionLifecycle = "closed" | "opening" | "open" | "closing"

export interface OperationOptions {
  signal?: AbortSignal
}

export interface DiffOptions extends OperationOptions {
  rows?: boolean
  staged?: boolean
  root?: string
  from?: string
  to?: string
  path?: string
}

export interface HistoryOptions extends OperationOptions {
  limit?: number
  after?: string
}

export interface RestoreOptions extends OperationOptions {
  source?: string
  expectedHead?: string
  requireClean?: boolean
  path: string
}

export interface RemoteConfigureOptions extends OperationOptions {
  name: string
  url: string
  bearerToken?: string
  overwrite?: boolean
  upstreamBranch?: string
}

export interface RemoteOperationOptions extends OperationOptions {
  remote?: string
  branch?: string
}

export interface CloneOptions extends OperationOptions {
  remoteUrl: string
  branch?: string
  bearerToken?: string
}

export type GraftJson = Record<string, unknown> | unknown[]

export class GraftSdkError extends Error {
  readonly code: string
  readonly cause?: unknown
}

export class RepositorySession {
  constructor(target: string)
  static open(
    target: string,
    options?: OperationOptions
  ): Promise<RepositorySession>

  readonly target: string
  readonly lifecycle: SessionLifecycle

  open(options?: OperationOptions): Promise<SessionLifecycle>
  close(options?: OperationOptions): Promise<SessionLifecycle>
  reopen(options?: OperationOptions): Promise<SessionLifecycle>

  setHttpBearerToken(remoteName: string, token: string): void
  clearHttpBearerToken(remoteName: string): void

  init(options?: OperationOptions): Promise<GraftJson>
  status(options?: OperationOptions): Promise<GraftJson>
  addAll(options?: OperationOptions): Promise<GraftJson>
  commit(message: string, options?: OperationOptions): Promise<GraftJson>
  diff(options?: DiffOptions): Promise<GraftJson>
  history(options?: HistoryOptions): Promise<GraftJson>
  restore(options: RestoreOptions): Promise<GraftJson>
  configureRemote(options: RemoteConfigureOptions): Promise<GraftJson>
  push(options?: RemoteOperationOptions): Promise<GraftJson>
  fetch(options?: RemoteOperationOptions): Promise<GraftJson>
  pull(options?: RemoteOperationOptions): Promise<GraftJson>
  cloneRepository(options: CloneOptions): Promise<GraftJson>
}

export function operationMaterializesWorktree(operation: string): boolean
export function sdkVersion(): string
