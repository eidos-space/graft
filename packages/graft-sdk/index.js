"use strict"

const fs = require("node:fs")
const path = require("node:path")

const native = loadNativeBinding()

class GraftSdkError extends Error {
  constructor(message, code, cause) {
    super(message, { cause })
    this.name = "GraftSdkError"
    this.code = code
  }
}

class RepositorySession {
  #native

  constructor(target, { identity } = {}) {
    this.#native = new native.RepositorySession(path.resolve(target), identity)
  }

  static async open(target, options = {}) {
    const { identity, signal } = options
    const session = new RepositorySession(target, { identity })
    await session.open({ signal })
    return session
  }

  get target() {
    return this.#native.target
  }

  get lifecycle() {
    return this.#native.lifecycle
  }

  async open({ signal } = {}) {
    return call(() => this.#native.open(signal))
  }

  async close({ signal } = {}) {
    return call(() => this.#native.close(signal))
  }

  async reopen({ signal } = {}) {
    return call(() => this.#native.reopen(signal))
  }

  setHttpBearerToken(remoteName, token) {
    return callSync(() => this.#native.setHttpBearerToken(remoteName, token))
  }

  clearHttpBearerToken(remoteName) {
    return callSync(() => this.#native.clearHttpBearerToken(remoteName))
  }

  async init({ signal } = {}) {
    return callJson(() => this.#native.init(signal))
  }

  async status({ signal } = {}) {
    return callJson(() => this.#native.status(signal))
  }

  async statusIncremental({ signal } = {}) {
    return callJson(() => this.#native.statusIncremental(signal))
  }

  async repositoryMetadata({ signal } = {}) {
    return callJson(() => this.#native.repositoryMetadata(signal))
  }

  async listRemotes({ signal } = {}) {
    return callJson(() => this.#native.listRemotes(signal))
  }

  async configGet(key, { signal } = {}) {
    return callJson(() => this.#native.configGet(key, signal))
  }

  async configList({ signal } = {}) {
    return callJson(() => this.#native.configList(signal))
  }

  async configSet(key, value, { signal } = {}) {
    return callJson(() => this.#native.configSet(key, value, signal))
  }

  async configUnset(key, { signal } = {}) {
    return callJson(() => this.#native.configUnset(key, signal))
  }

  async addAll({ signal } = {}) {
    return callJson(() => this.#native.addAll(signal))
  }

  async stagePaths(options) {
    const { signal, ...stageOptions } = options
    return callJson(() => this.#native.stagePaths(stageOptions, signal))
  }

  async captureSqliteSnapshot(options) {
    const { signal, ...captureOptions } = options
    return callJson(() =>
      this.#native.captureSqliteSnapshot(captureOptions, signal)
    )
  }

  async recordPathMove(options) {
    const { signal, ...moveOptions } = options
    return callJson(() => this.#native.recordPathMove(moveOptions, signal))
  }

  async untrackPaths(options) {
    const { signal, ...untrackOptions } = options
    return callJson(() => this.#native.untrackPaths(untrackOptions, signal))
  }

  async commit(message, { signal } = {}) {
    return callJson(() => this.#native.commit(message, signal))
  }

  async diff(options = {}) {
    const { signal, ...diffOptions } = options
    return callJson(() => this.#native.diff(diffOptions, signal))
  }

  async diffPaths(options) {
    const { signal, ...diffOptions } = options
    return callJson(() => this.#native.diffPaths(diffOptions, signal))
  }

  async diffSqlitePaths(options) {
    const { signal, ...diffOptions } = options
    return callJson(() => this.#native.diffSqlitePaths(diffOptions, signal))
  }

  async readPathContent(options) {
    const { signal, ...readOptions } = options
    return callJson(() => this.#native.readPathContent(readOptions, signal))
  }

  async history(options = {}) {
    const { limit = 50, after, signal } = options
    return callJson(() => this.#native.history(limit, after, signal))
  }

  async historySummaries(options = {}) {
    const { limit = 50, after, signal } = options
    return callJson(() => this.#native.historySummaries(limit, after, signal))
  }

  async commitDetails(revision, { signal } = {}) {
    return callJson(() => this.#native.commitDetails(revision, signal))
  }

  async commitChangedPaths(options) {
    const { signal, ...pathOptions } = options
    return callJson(() => this.#native.commitChangedPaths(pathOptions, signal))
  }

  async isIgnoredPath(path, { signal } = {}) {
    return callJson(() => this.#native.isIgnoredPath(path, signal))
  }

  async isIgnoredPaths(options) {
    const { signal, ...pathOptions } = options
    return callJson(() => this.#native.isIgnoredPaths(pathOptions, signal))
  }

  async inventory(options = {}) {
    const { kind = "tracked_ignored", limit = 100, after, signal } = options
    return callJson(() => this.#native.inventory({ kind, limit, after }, signal))
  }

  async restore(options) {
    const { signal, ...restoreOptions } = options
    return callJson(() => this.#native.restore(restoreOptions, signal))
  }

  async restorePaths(options) {
    const { signal, ...restoreOptions } = options
    return callJson(() => this.#native.restorePaths(restoreOptions, signal))
  }

  async configureRemote(options) {
    const { signal, ...remoteOptions } = options
    return callJson(() => this.#native.configureRemote(remoteOptions, signal))
  }

  async push(options = {}) {
    const { remote, branch, signal, onProgress } = options
    return callJsonWithProgress(
      () => this.#native.push(remote, branch, signal, onProgress),
      onProgress
    )
  }

  async fetch(options = {}) {
    const { remote, branch, signal, onProgress } = options
    return callJsonWithProgress(
      () => this.#native.fetch(remote, branch, signal, onProgress),
      onProgress
    )
  }

  async pull(options = {}) {
    const { remote, branch, signal, onProgress } = options
    return callJsonWithProgress(
      () => this.#native.pull(remote, branch, signal, onProgress),
      onProgress
    )
  }

  async getMergePolicy({ signal } = {}) {
    signal?.throwIfAborted()
    return callJson(() => this.#native.getMergePolicy(signal))
  }

  async validateMergePolicy({ policy, signal }) {
    signal?.throwIfAborted()
    return callJson(() =>
      this.#native.validateMergePolicy(JSON.stringify(policy), signal)
    )
  }

  async setMergePolicy({ policy, expectedPolicyToken, signal }) {
    signal?.throwIfAborted()
    return callJson(() =>
      this.#native.setMergePolicy(
        {
          policyJson: JSON.stringify(policy),
          expectedPolicyToken,
        },
        signal
      )
    )
  }

  async planMerge(options) {
    const { signal, ...mergeOptions } = options
    return callJson(() => this.#native.planMerge(mergeOptions, signal))
  }

  async applyMerge(options) {
    const { signal, onProgress, ...mergeOptions } = options
    return callJsonWithProgress(
      () => this.#native.applyMerge(mergeOptions, signal, onProgress),
      onProgress
    )
  }

  async getMergeStatus({ signal } = {}) {
    return callJson(() => this.#native.getMergeStatus(signal))
  }

  async listMergePaths(options) {
    const {
      signal,
      filter = "all",
      limit = 100,
      after,
      expectedStateToken,
    } = options
    return callJson(() =>
      this.#native.listMergePaths(
        {
          filter,
          limit,
          after,
          expectedStateToken,
        },
        signal
      )
    )
  }

  async listMergeConflicts(options) {
    const {
      signal,
      path: conflictPath,
      limit = 100,
      after,
      expectedStateToken,
    } = options
    return callJson(() =>
      this.#native.listMergeConflicts(
        {
          path: conflictPath,
          limit,
          after,
          expectedStateToken,
        },
        signal
      )
    )
  }

  async readMergeVersion(options) {
    const { signal, ...readOptions } = options
    return callJson(() =>
      this.#native.readMergeVersion(readOptions, signal)
    )
  }

  async diffMergeSqlite(options) {
    const { signal, ...diffOptions } = options
    return callJson(() =>
      this.#native.diffMergeSqlite(diffOptions, signal)
    )
  }

  async setMergePathResult(options) {
    const { signal, ...resultOptions } = options
    return callJson(() =>
      this.#native.setMergePathResult(resultOptions, signal)
    )
  }

  async resolveMergeRow(options) {
    const { signal, identity, ...rowOptions } = options
    return callJson(() =>
      this.#native.resolveMergeRow(
        {
          ...rowOptions,
          identity: JSON.stringify(identity),
        },
        signal
      )
    )
  }

  async resolveMergeCell(options) {
    const { signal, identity, ...cellOptions } = options
    return callJson(() =>
      this.#native.resolveMergeCell(
        {
          ...cellOptions,
          identity: JSON.stringify(identity),
        },
        signal
      )
    )
  }

  async resolveMergeTable(options) {
    const { signal, ...tableOptions } = options
    return callJson(() =>
      this.#native.resolveMergeTable(tableOptions, signal)
    )
  }

  async stageMergeSqliteResult(options) {
    const { signal, ...resultOptions } = options
    return callJson(() =>
      this.#native.stageMergeSqliteResult(resultOptions, signal)
    )
  }

  async prepareSemanticMerge(options) {
    const { signal, managedTables = [], ...providerOptions } = options
    return callJson(() =>
      this.#native.prepareSemanticMerge(
        { ...providerOptions, managedTables },
        signal
      )
    )
  }

  async recordSemanticMergeConflicts(options) {
    const {
      signal,
      providerToken,
      conflicts,
      automaticResolutions = [],
      expectedStateToken,
    } = options
    return callJson(() =>
      this.#native.recordSemanticMergeConflicts(
        {
          providerToken,
          conflictsJson: JSON.stringify(conflicts),
          automaticResolutionsJson: JSON.stringify(automaticResolutions),
          expectedStateToken,
        },
        signal
      )
    )
  }

  async acceptSemanticMergeResult(options) {
    const {
      signal,
      providerToken,
      validation,
      automaticResolutions = [],
      expectedStateToken,
    } = options
    return callJson(() =>
      this.#native.acceptSemanticMergeResult(
        {
          providerToken,
          validationJson: JSON.stringify(validation),
          automaticResolutionsJson: JSON.stringify(automaticResolutions),
          expectedStateToken,
        },
        signal
      )
    )
  }

  async unresolveMergePath(options) {
    const { signal, ...pathOptions } = options
    return callJson(() =>
      this.#native.unresolveMergePath(pathOptions, signal)
    )
  }

  async writeAndStageTextResult(options) {
    const { signal, ...resultOptions } = options
    return callJson(() =>
      this.#native.writeAndStageTextResult(resultOptions, signal)
    )
  }

  async continueMerge(options) {
    const { signal, ...continueOptions } = options
    return callJson(() =>
      this.#native.continueMerge(continueOptions, signal)
    )
  }

  async abortMerge(options) {
    const { signal, ...abortOptions } = options
    return callJson(() =>
      this.#native.abortMerge(abortOptions, signal)
    )
  }

  async cloneRepository(options) {
    const { signal, onProgress, ...cloneOptions } = options
    return callJsonWithProgress(
      () => this.#native.cloneRepository(cloneOptions, signal, onProgress),
      onProgress
    )
  }
}

function operationMaterializesWorktree(operation) {
  return callSync(() => native.operationMaterializesWorktree(operation))
}

function sdkVersion() {
  return native.sdkVersion()
}

async function call(operation) {
  try {
    return await operation()
  } catch (error) {
    throw normalizeError(error)
  }
}

async function callJson(operation) {
  const encoded = await call(operation)
  try {
    return JSON.parse(encoded)
  } catch (error) {
    throw new GraftSdkError(
      "Graft SDK native binding returned invalid JSON",
      "GRAFT_SDK_INVALID_RESPONSE",
      error
    )
  }
}

async function callJsonWithProgress(operation, onProgress) {
  try {
    return await callJson(operation)
  } finally {
    if (typeof onProgress === "function") {
      await new Promise((resolve) => setImmediate(resolve))
    }
  }
}

function callSync(operation) {
  try {
    return operation()
  } catch (error) {
    throw normalizeError(error)
  }
}

function normalizeError(error) {
  if (error instanceof GraftSdkError) return error
  if (error?.name === "AbortError" || error?.message === "AbortError") {
    return error
  }
  const message = error instanceof Error ? error.message : String(error)
  const match = message.match(/^\[(GRAFT_SDK_[A-Z_]+)\]\s*(.*)$/s)
  if (!match) {
    return new GraftSdkError(message, "GRAFT_SDK_NATIVE", error)
  }
  if (match[1] === "GRAFT_SDK_CANCELLED") {
    const cancelled = new Error(match[2], { cause: error })
    cancelled.name = "AbortError"
    return cancelled
  }
  return new GraftSdkError(match[2], match[1], error)
}

function loadNativeBinding() {
  const configured = process.env.GRAFT_SDK_NATIVE_PATH
    ? path.resolve(process.env.GRAFT_SDK_NATIVE_PATH)
    : undefined
  const platformName = nativePlatformName()
  const candidates = [
    configured,
    path.join(__dirname, "native", `graft-sdk.${platformName}.node`),
    path.join(__dirname, `graft-sdk.${platformName}.node`),
  ].filter(Boolean)
  const bindingPath = candidates.find((candidate) => fs.existsSync(candidate))
  if (bindingPath) return require(bindingPath)

  const packageName = `@eidos.space/graft-${platformName}`
  try {
    return require(packageName)
  } catch (error) {
    const detail = error instanceof Error ? `: ${error.message}` : ""
    throw new Error(
      `Graft SDK native binding is unavailable for ${platformName}; install ${packageName}${detail}`,
      { cause: error }
    )
  }
}

function nativePlatformName() {
  if (process.platform === "darwin") {
    if (process.arch === "arm64" || process.arch === "x64") {
      return `darwin-${process.arch}`
    }
  }
  if (process.platform === "win32" && process.arch === "x64") {
    return "win32-x64-msvc"
  }
  if (process.platform === "linux") {
    if (isMusl()) {
      throw new Error(
        "Graft SDK 0.1 does not provide Linux musl binaries; use a glibc-based Node.js runtime"
      )
    }
    if (process.arch === "arm64" || process.arch === "x64") {
      return `linux-${process.arch}-gnu`
    }
  }
  throw new Error(
    `Graft SDK does not support ${process.platform}-${process.arch}`
  )
}

function isMusl() {
  if (typeof process.report?.getReport !== "function") return false
  return !process.report.getReport().header?.glibcVersionRuntime
}

module.exports = {
  GraftSdkError,
  RepositorySession,
  operationMaterializesWorktree,
  sdkVersion,
}
