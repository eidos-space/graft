import assert from "node:assert/strict";
import { test } from "node:test";
import { parsePreferences, readPreference, writePreference } from "../src/preferences.ts";

test("preference parser validates browser values and clamps sidebar width", () => {
  assert.deepEqual(parsePreferences('{"compactFolders":false}'), { compactFolders: false });
  assert.deepEqual(parsePreferences('{"compactFolders":"false"}'), {});
  assert.deepEqual(parsePreferences('{"theme":"light","layout":{"side":"left","width":900}}'), {
    theme: "light",
    layout: { side: "left", width: 640 },
  });
  assert.deepEqual(
    parsePreferences('{"theme":"invalid","layout":{"side":"left","width":"300"}}'),
    {},
  );
  assert.deepEqual(parsePreferences("broken"), {});
  assert.deepEqual(parsePreferences("null"), {});
});
test("display preferences migrate from local storage and cookie takes precedence across origins", () => {
  const entries = new Map([
    ["graft-theme", "dark"],
    ["graft-workspace-layout", '{"side":"right","width":330}'],
  ]);
  let cookie = "";
  const priorDocument = Object.getOwnPropertyDescriptor(globalThis, "document");
  const priorStorage = Object.getOwnPropertyDescriptor(globalThis, "localStorage");
  Object.defineProperty(globalThis, "document", {
    configurable: true,
    value: {
      get cookie() {
        return cookie;
      },
      set cookie(value: string) {
        cookie = value.split(";")[0];
      },
    },
  });
  Object.defineProperty(globalThis, "localStorage", {
    configurable: true,
    value: {
      getItem: (key: string) => entries.get(key) ?? null,
      setItem: (key: string, value: string) => entries.set(key, value),
    },
  });
  try {
    assert.equal(readPreference("theme"), "dark");
    writePreference("theme", "light");
    writePreference("layout", { side: "left", width: 280 });
    writePreference("compactFolders", false);
    entries.clear(); // Another origin has no localStorage entries but the same host cookie.
    assert.equal(readPreference("theme"), "light");
    assert.equal(readPreference("compactFolders"), false);
    assert.deepEqual(readPreference("layout"), { side: "left", width: 280 });
  } finally {
    if (priorDocument) Object.defineProperty(globalThis, "document", priorDocument);
    else Reflect.deleteProperty(globalThis, "document");
    if (priorStorage) Object.defineProperty(globalThis, "localStorage", priorStorage);
    else Reflect.deleteProperty(globalThis, "localStorage");
  }
});
