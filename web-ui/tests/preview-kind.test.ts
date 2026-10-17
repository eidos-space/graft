import assert from "node:assert/strict";
import { test } from "node:test";
import { previewKind } from "../src/previewKind.ts";

test("text formats always use the source diff/editor", () => {
  for (const path of [
    "index.html",
    "page.HTM",
    "icon.svg",
    "data.xml",
    "README.md",
    "app.ts",
    "notes.txt",
  ])
    assert.equal(previewKind(path), undefined, path);
});

test("binary browser media retain dedicated previews", () => {
  assert.equal(previewKind("image.PNG"), "image");
  assert.equal(previewKind("clip.webm"), "video");
  assert.equal(previewKind("audio.mp3"), "audio");
  assert.equal(previewKind("document.pdf"), "pdf");
});
