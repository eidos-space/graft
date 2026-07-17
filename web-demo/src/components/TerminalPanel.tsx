import { Terminal, useTerminal } from "@wterm/react";
import {
  forwardRef,
  useCallback,
  useEffect,
  useImperativeHandle,
  useRef,
  useState,
} from "react";
import { type Translate, useI18n } from "../i18n";
import type { GraftClient } from "../lib/graftClient";
import {
  createOpfsDirectory,
  listOpfsDirectory,
  readOpfsText,
  removeOpfsPath,
  resolveOpfsPath,
  statOpfsPath,
  touchOpfsFile,
  writeOpfsBlob,
  writeOpfsText,
} from "../lib/opfs";

const MAX_DOWNLOAD_BYTES = 10 * 1024 * 1024;

function parseCommand(line: string, t: Translate): string[] {
  const args: string[] = [];
  let current = "";
  let quote: "'" | '"' | null = null;
  let escaped = false;

  for (const char of line.trim()) {
    if (escaped) {
      current += char;
      escaped = false;
    } else if (char === "\\" && quote !== "'") {
      escaped = true;
    } else if (quote) {
      if (char === quote) quote = null;
      else current += char;
    } else if (char === "'" || char === '"') {
      quote = char;
    } else if (/\s/.test(char)) {
      if (current) args.push(current);
      current = "";
    } else {
      current += char;
    }
  }
  if (escaped) current += "\\";
  if (quote) throw new Error(t("terminal.unclosedQuote", { quote }));
  if (current) args.push(current);
  return args;
}

function formatBytes(bytes = 0) {
  if (bytes < 1024) return `${bytes}B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)}K`;
  return `${(bytes / 1024 / 1024).toFixed(1)}M`;
}

interface TerminalPanelProps {
  client: GraftClient;
  onCommandComplete: () => Promise<void> | void;
  onOpenFile: (path: string) => void;
  runtimeReady: boolean;
}

export interface TerminalPanelHandle {
  run: (command: string) => Promise<boolean>;
}

export const TerminalPanel = forwardRef<TerminalPanelHandle, TerminalPanelProps>(
  function TerminalPanel(
    { client, onCommandComplete, onOpenFile, runtimeReady },
    forwardedRef,
  ) {
  const { locale, t } = useI18n();
  const translateRef = useRef(t);
  translateRef.current = t;
  const { focus, ref, write } = useTerminal();
  const line = useRef("");
  const busy = useRef(false);
  const cwd = useRef("/");
  const previousCwd = useRef("/");
  const [terminalReady, setTerminalReady] = useState(false);
  const history = useRef<string[]>([]);
  const historyIndex = useRef(0);

  const prompt = useCallback(() => {
    const location = cwd.current === "/" ? "/" : cwd.current;
    return `\x1b[38;2;188;82;21m${location}\x1b[0m \x1b[1m$\x1b[0m `;
  }, []);

  const redrawLine = useCallback(
    (nextLine: string) => {
      line.current = nextLine;
      write(`\r\x1b[2K${prompt()}${nextLine}`);
    },
    [prompt, write],
  );

  const printPrompt = useCallback(() => {
    line.current = "";
    write(prompt());
  }, [prompt, write]);

  const printLines = useCallback(
    (lines: string[]) => {
      for (const output of lines) write(`${output}\r\n`);
    },
    [write],
  );

  const runShellCommand = useCallback(
    async (args: string[]) => {
      const command = args.shift()?.toLowerCase();
      if (!command) return false;

      if (command === "help") {
        printLines(t("terminal.help").split("\n"));
        return false;
      }
      if (command === "pwd") {
        printLines([cwd.current]);
        return false;
      }
      if (command === "whoami") {
        printLines(["developer"]);
        return false;
      }
      if (command === "date") {
        printLines([
          new Intl.DateTimeFormat(locale, {
            dateStyle: "full",
            timeStyle: "medium",
          }).format(new Date()),
        ]);
        return false;
      }
      if (command === "history") {
        printLines(
          history.current.map((entry, index) => `${String(index + 1).padStart(4)}  ${entry}`),
        );
        return false;
      }
      if (command === "cd") {
        const target = args[0] === "-" ? previousCwd.current : (args[0] ?? "/");
        const absolute = target === "~" ? "/" : resolveOpfsPath(target, cwd.current);
        const stat = await statOpfsPath(absolute);
        if (stat.kind !== "directory") {
          throw new Error(t("terminal.notDirectory", { path: target }));
        }
        previousCwd.current = cwd.current;
        cwd.current = absolute;
        return false;
      }
      if (command === "ls") {
        const options = args.filter((arg) => arg.startsWith("-"));
        const showAll = options.some((option) => option.includes("a"));
        const long = options.some((option) => option.includes("l"));
        const targets = args.filter((arg) => !arg.startsWith("-"));
        for (const [targetIndex, target] of (targets.length ? targets : ["."]).entries()) {
          const absolute = resolveOpfsPath(target, cwd.current);
          const stat = await statOpfsPath(absolute);
          if (targets.length > 1) printLines([`${target}:`]);
          if (stat.kind === "file") {
            printLines([long ? `-rw-r--r--  ${formatBytes(stat.size).padStart(7)}  ${target}` : target]);
          } else {
            const entries = (await listOpfsDirectory(absolute)).filter(
              (entry) => showAll || !entry.name.startsWith("."),
            );
            printLines(
              entries.map((entry) => {
                const name =
                  entry.kind === "directory"
                    ? `\x1b[34;1m${entry.name}/\x1b[0m`
                    : entry.name;
                return long
                  ? `${entry.kind === "directory" ? "drwxr-xr-x" : "-rw-r--r--"}  ${formatBytes(entry.size).padStart(7)}  ${name}`
                  : name;
              }),
            );
          }
          if (targetIndex < Math.max(targets.length, 1) - 1) printLines([""]);
        }
        return false;
      }
      if (command === "cat") {
        if (!args.length) throw new Error(t("terminal.catMissing"));
        for (const path of args) printLines((await readOpfsText(path, cwd.current)).split("\n"));
        return false;
      }
      if (command === "open") {
        const path = args[0];
        if (!path) throw new Error(t("terminal.openMissing"));
        const stat = await statOpfsPath(path, cwd.current);
        if (stat.kind !== "file") throw new Error(t("terminal.notFile", { path }));
        onOpenFile(stat.path.slice(1));
        return false;
      }
      if (command === "mkdir") {
        const recursive = args.some((arg) => arg === "-p" || arg === "--parents");
        const paths = args.filter((arg) => !arg.startsWith("-"));
        if (!paths.length) throw new Error(t("terminal.mkdirMissing"));
        for (const path of paths) await createOpfsDirectory(path, cwd.current, recursive);
        return true;
      }
      if (command === "touch") {
        if (!args.length) throw new Error(t("terminal.touchMissing"));
        for (const path of args) await touchOpfsFile(path, cwd.current);
        return true;
      }
      if (command === "echo") {
        const redirectIndex = args.findIndex((arg) => arg === ">" || arg === ">>");
        if (redirectIndex < 0) {
          printLines([args.join(" ")]);
          return false;
        }
        const operator = args[redirectIndex];
        const path = args[redirectIndex + 1];
        if (!path) throw new Error(t("terminal.redirectMissing"));
        const next = `${args.slice(0, redirectIndex).join(" ")}\n`;
        const previous =
          operator === ">>"
            ? await readOpfsText(path, cwd.current).catch(() => "")
            : "";
        await writeOpfsText(path, `${previous}${next}`, cwd.current);
        return true;
      }
      if (command === "curl") {
        let createDirectories = false;
        let outputPath: string | undefined;
        let url: string | undefined;

        for (let index = 0; index < args.length; index += 1) {
          const argument = args[index];
          if (argument === "--create-dirs") {
            createDirectories = true;
          } else if (argument === "-o" || argument === "--output") {
            outputPath = args[index + 1];
            index += 1;
          } else if (argument.startsWith("--output=")) {
            outputPath = argument.slice("--output=".length);
          } else if (argument.startsWith("-o") && argument.length > 2) {
            outputPath = argument.slice(2);
          } else if (
            argument === "--location" ||
            argument === "--fail" ||
            argument === "--silent" ||
            argument === "--show-error" ||
            /^-[fLsS]+$/.test(argument)
          ) {
            continue;
          } else if (argument.startsWith("-")) {
            throw new Error(t("terminal.curlUnsupportedOption", { option: argument }));
          } else if (!url) {
            url = argument;
          }
        }

        if (!url) throw new Error(t("terminal.curlMissingUrl"));
        if (!outputPath) throw new Error(t("terminal.curlMissingOutput"));

        let parsedUrl: URL;
        try {
          parsedUrl = new URL(url, window.location.origin);
        } catch {
          throw new Error(t("terminal.curlInvalidUrl", { url }));
        }
        if (parsedUrl.protocol !== "https:" && parsedUrl.protocol !== "http:") {
          throw new Error(t("terminal.curlUnsupportedProtocol", { protocol: parsedUrl.protocol }));
        }

        const controller = new AbortController();
        const timeout = window.setTimeout(() => controller.abort(), 20_000);
        try {
          const response = await fetch(parsedUrl, {
            redirect: "follow",
            signal: controller.signal,
          });
          if (!response.ok) {
            throw new Error(
              t("terminal.curlHttpError", {
                status: response.status,
                statusText: response.statusText,
              }),
            );
          }
          const declaredSize = Number(response.headers.get("content-length"));
          if (Number.isFinite(declaredSize) && declaredSize > MAX_DOWNLOAD_BYTES) {
            throw new Error(
              t("terminal.curlTooLarge", { limit: formatBytes(MAX_DOWNLOAD_BYTES) }),
            );
          }
          const blob = await response.blob();
          if (blob.size > MAX_DOWNLOAD_BYTES) {
            throw new Error(
              t("terminal.curlTooLarge", { limit: formatBytes(MAX_DOWNLOAD_BYTES) }),
            );
          }
          await writeOpfsBlob(outputPath, blob, cwd.current, createDirectories);
          printLines([
            t("terminal.curlSaved", {
              path: resolveOpfsPath(outputPath, cwd.current),
              size: formatBytes(blob.size),
            }),
          ]);
          return true;
        } catch (error) {
          if (
            error instanceof TypeError ||
            (error instanceof DOMException && error.name === "AbortError")
          ) {
            throw new Error(t("terminal.curlNetworkError", { url }));
          }
          throw error;
        } finally {
          window.clearTimeout(timeout);
        }
      }
      if (command === "rm") {
        const recursive = args.some((arg) => arg.startsWith("-") && arg.includes("r"));
        const force = args.some((arg) => arg.startsWith("-") && arg.includes("f"));
        const paths = args.filter((arg) => !arg.startsWith("-"));
        if (!paths.length) throw new Error(t("terminal.rmMissing"));
        for (const path of paths) {
          try {
            const stat = await statOpfsPath(path, cwd.current);
            if (stat.kind === "directory" && !recursive) {
              throw new Error(t("terminal.isDirectory", { path }));
            }
            await removeOpfsPath(path, cwd.current, recursive);
          } catch (error) {
            if (!force) throw error;
          }
        }
        return true;
      }

      throw new Error(t("terminal.notFound", { command }));
    },
    [locale, onOpenFile, printLines, t],
  );

  const execute = useCallback(
    async (rawLine: string) => {
      const trimmed = rawLine.trim();
      if (!trimmed) {
        printPrompt();
        return true;
      }
      history.current.push(trimmed);
      historyIndex.current = history.current.length;

      if (trimmed === "clear") {
        write("\x1b[2J\x1b[H");
        printPrompt();
        return true;
      }

      let args: string[];
      try {
        args = parseCommand(trimmed, t);
      } catch (error) {
        write(`\x1b[31m${String(error)}\x1b[0m\r\n`);
        printPrompt();
        return false;
      }

      busy.current = true;
      let changed = false;
      let succeeded = true;
      try {
        if (args[0] === "graft") {
          args.shift();
          if (args.length === 0) args = ["--help"];
          const result = await client.run(
            ["--browser-cwd", cwd.current, ...args],
            ({ line: output, stream }) => {
              const color = stream === "stderr" ? "\x1b[31m" : "";
              const reset = stream === "stderr" ? "\x1b[0m" : "";
              write(`${color}${output}${reset}\r\n`);
            },
          );
          succeeded = result.code === 0;
          changed = succeeded;
        } else {
          changed = await runShellCommand(args);
        }
      } catch (error) {
        succeeded = false;
        const detail = error instanceof Error ? error.message : String(error);
        write(`\x1b[31m${detail}\x1b[0m\r\n`);
      } finally {
        busy.current = false;
        if (changed) {
          try {
            await onCommandComplete();
          } catch (error) {
            succeeded = false;
            const detail = error instanceof Error ? error.message : String(error);
            write(`\x1b[31m${detail}\x1b[0m\r\n`);
          }
        }
        printPrompt();
        focus();
      }
      return succeeded;
    },
    [client, focus, onCommandComplete, printPrompt, runShellCommand, t, write],
  );

  useImperativeHandle(
    forwardedRef,
    () => ({
      run: async (command: string) => {
        if (!runtimeReady || !terminalReady || busy.current) return false;
        line.current = "";
        write(`${command}\r\n`);
        return execute(command);
      },
    }),
    [execute, runtimeReady, terminalReady, write],
  );

  const handleData = useCallback(
    (data: string) => {
      if (busy.current || !runtimeReady) return;

      if (data === "\x1b[A" || data === "\x1b[B") {
        const direction = data === "\x1b[A" ? -1 : 1;
        historyIndex.current = Math.max(
          0,
          Math.min(history.current.length, historyIndex.current + direction),
        );
        redrawLine(history.current[historyIndex.current] ?? "");
        return;
      }

      for (const char of data) {
        if (char === "\r" || char === "\n") {
          const command = line.current;
          line.current = "";
          write("\r\n");
          void execute(command);
          return;
        }
        if (char === "\x7f" || char === "\b") {
          if (line.current) {
            line.current = line.current.slice(0, -1);
            write("\b \b");
          }
          continue;
        }
        if (char >= " " && char !== "\x7f") {
          line.current += char;
          write(char);
        }
      }
    },
    [execute, redrawLine, runtimeReady, write],
  );

  useEffect(() => {
    if (!runtimeReady || !terminalReady) return;
    write(`\x1b[32m${translateRef.current("terminal.runtimeReady")}\x1b[0m\r\n`);
    write(`${translateRef.current("terminal.shellReady")}\r\n\r\n`);
    printPrompt();
    focus();
  }, [focus, printPrompt, runtimeReady, terminalReady, write]);

  return (
    <Terminal
      ref={ref}
      aria-label={t("terminal.label")}
      autoResize
      className="graft-terminal"
      cursorBlink
      onData={handleData}
      onReady={() => {
        setTerminalReady(true);
        write(`\x1b[1m${t("terminal.workbench")}\x1b[0m\r\n`);
        write(`${t("terminal.capabilities")}\r\n\r\n`);
        if (!runtimeReady) write(`${t("terminal.loading")}\r\n`);
      }}
      rows={14}
      theme="light"
    />
  );
  },
);
