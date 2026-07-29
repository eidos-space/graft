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

  constructor(target) {
    this.#native = new native.RepositorySession(path.resolve(target))
  }

  static async open(target, options = {}) {
    const session = new RepositorySession(target)
    await session.open(options)
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

  async addAll({ signal } = {}) {
    return callJson(() => this.#native.addAll(signal))
  }

  async commit(message, { signal } = {}) {
    return callJson(() => this.#native.commit(message, signal))
  }

  async diff(options = {}) {
    const { signal, ...diffOptions } = options
    return callJson(() => this.#native.diff(diffOptions, signal))
  }

  async history(options = {}) {
    const { limit = 50, after, signal } = options
    return callJson(() => this.#native.history(limit, after, signal))
  }

  async restore(options) {
    const { signal, ...restoreOptions } = options
    return callJson(() => this.#native.restore(restoreOptions, signal))
  }

  async configureRemote(options) {
    const { signal, ...remoteOptions } = options
    return callJson(() => this.#native.configureRemote(remoteOptions, signal))
  }

  async push(options = {}) {
    const { remote, branch, signal } = options
    return callJson(() => this.#native.push(remote, branch, signal))
  }

  async fetch(options = {}) {
    const { remote, branch, signal } = options
    return callJson(() => this.#native.fetch(remote, branch, signal))
  }

  async pull(options = {}) {
    const { remote, branch, signal } = options
    return callJson(() => this.#native.pull(remote, branch, signal))
  }

  async cloneRepository(options) {
    const { signal, ...cloneOptions } = options
    return callJson(() => this.#native.cloneRepository(cloneOptions, signal))
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
