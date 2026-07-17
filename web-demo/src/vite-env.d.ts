/// <reference types="vite/client" />

declare module "/wasm/graft.js" {
  type GraftModule = {
    callMain(args: string[]): number;
    stackRestore(pointer: number): void;
    stackSave(): number;
  };

  export default function createGraft(options?: {
    locateFile?: (path: string) => string;
    print?: (line: string) => void;
    printErr?: (line: string) => void;
    noInitialRun?: boolean;
    thisProgram?: string;
  }): Promise<GraftModule>;
}
