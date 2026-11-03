import { once } from "node:events";

// Register before kill: Windows can terminate immediately. A child that has
// already exited will never emit another exit event.
export async function stopChildProcess(child) {
  if (child.exitCode !== null || child.signalCode !== null) return;
  const exited = once(child, "exit");
  child.kill();
  await exited;
}
