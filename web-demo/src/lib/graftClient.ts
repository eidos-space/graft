import type { CommandOutput, CommandResult } from "../types";

interface PendingCommand {
  onOutput?: (output: CommandOutput) => void;
  reject: (error: Error) => void;
  resolve: (result: CommandResult) => void;
}

type WorkerMessage =
  | { type: "ready" }
  | ({ id: number; type: "output" } & CommandOutput)
  | { id: number; result: CommandResult; type: "result" };

export class GraftClient {
  readonly ready: Promise<void>;
  private nextId = 1;
  private pending = new Map<number, PendingCommand>();
  private resolveReady!: () => void;
  private rejectReady!: (error: Error) => void;
  private worker: Worker;

  constructor() {
    this.ready = new Promise<void>((resolve, reject) => {
      this.resolveReady = resolve;
      this.rejectReady = reject;
    });
    this.worker = new Worker(new URL("../graft.worker.ts", import.meta.url), {
      name: "graft-cli",
      type: "module",
    });
    this.worker.addEventListener("message", this.handleMessage);
    this.worker.addEventListener("error", this.handleError);
  }

  async run(
    args: string[],
    onOutput?: (output: CommandOutput) => void,
  ): Promise<CommandResult> {
    await this.ready;
    const id = this.nextId++;
    return new Promise<CommandResult>((resolve, reject) => {
      this.pending.set(id, { onOutput, reject, resolve });
      this.worker.postMessage({ args, id, type: "run" });
    });
  }

  terminate() {
    this.worker.terminate();
    for (const command of this.pending.values()) {
      command.reject(new Error("Graft worker terminated"));
    }
    this.pending.clear();
  }

  private handleMessage = (event: MessageEvent<WorkerMessage>) => {
    const message = event.data;
    if (message.type === "ready") {
      this.resolveReady();
      return;
    }
    const command = this.pending.get(message.id);
    if (!command) return;
    if (message.type === "output") {
      command.onOutput?.({ line: message.line, stream: message.stream });
      return;
    }
    this.pending.delete(message.id);
    command.resolve(message.result);
  };

  private handleError = (event: ErrorEvent) => {
    const error = new Error(event.message || "Graft worker failed to start");
    this.rejectReady(error);
    for (const command of this.pending.values()) command.reject(error);
    this.pending.clear();
  };
}

export function parseJsonOutput<T>(result: CommandResult): T {
  if (result.code !== 0) {
    throw new Error(result.stderr.join("\n") || "Graft command failed");
  }
  const output = result.stdout.join("\n").trim();
  if (!output) throw new Error("Graft returned no JSON output");
  return JSON.parse(output) as T;
}
