/// <reference lib="webworker" />

import type { CommandOutput, CommandResult } from "./types";

interface RunMessage {
  args: string[];
  id: number;
  type: "run";
}

type GraftModule = {
  Asyncify: {
    currData: number | null;
    whenDone(): Promise<number>;
  };
  callMain(args: string[]): number | Promise<number>;
  stackRestore(pointer: number): void;
  stackSave(): number;
};

let activeCommandId: number | null = null;
let stdout: string[] = [];
let stderr: string[] = [];

function emit(stream: CommandOutput["stream"], line: string) {
  if (activeCommandId === null) return;
  const target = stream === "stdout" ? stdout : stderr;
  target.push(line);
  self.postMessage({ id: activeCommandId, line, stream, type: "output" });
}

const runtimeRoot = `${import.meta.env.BASE_URL}wasm/`;
const moduleUrl = `${runtimeRoot}graft.js`;
// Keep Vite from treating the generated Emscripten module as source. It lives in
// public/ so development and production serve the exact same generated file.
const importRuntime = new Function("url", "return import(url)") as (
  url: string,
) => Promise<{ default: (options: object) => Promise<unknown> }>;
const createGraft = (await importRuntime(moduleUrl)).default;

let graft: GraftModule;
try {
  graft = (await createGraft({
    locateFile: (path: string) =>
      path.endsWith(".wasm") ? `${runtimeRoot}graft.wasm` : `${runtimeRoot}${path}`,
    noInitialRun: true,
    print: (line: string) => emit("stdout", line),
    printErr: (line: string) => emit("stderr", line),
    thisProgram: "graft",
  })) as GraftModule;
} catch (error) {
  console.error("Failed to initialize Graft Wasm", error);
  throw error;
}

self.postMessage({ type: "ready" });

async function runCommand(message: RunMessage) {
  activeCommandId = message.id;
  stdout = [];
  stderr = [];
  let code = 1;
  const stackPointer = graft.stackSave();

  try {
    const initialCode = graft.callMain([...message.args]);
    if (graft.Asyncify.currData !== null) {
      code = await graft.Asyncify.whenDone();
    } else {
      code = await initialCode;
    }
  } catch (error) {
    emit(
      "stderr",
      error instanceof Error ? (error.stack ?? error.message) : String(error),
    );
  } finally {
    graft.stackRestore(stackPointer);
  }

  const result: CommandResult = { code, stderr, stdout };
  self.postMessage({ id: message.id, result, type: "result" });
  activeCommandId = null;
}

let commandQueue = Promise.resolve();
self.addEventListener("message", (event: MessageEvent<RunMessage>) => {
  if (event.data.type !== "run") return;
  commandQueue = commandQueue.then(() => runCommand(event.data));
});

export {};
