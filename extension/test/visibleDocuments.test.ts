import assert from "node:assert/strict";
import test from "node:test";
import type * as vscode from "vscode";
import { isShownDocument, newlyVisibleDocuments } from "../src/visibleDocuments.js";

const documentAt = (uri: string): vscode.TextDocument =>
  ({ uri: { toString: () => uri } }) as vscode.TextDocument;

const editorFor = (document: vscode.TextDocument): vscode.TextEditor =>
  ({ document }) as vscode.TextEditor;

test("only documents the previous set lacked are newly visible, each once", () => {
  const a = documentAt("file:///a.ts");
  const b = documentAt("file:///b.ts");

  const fresh = newlyVisibleDocuments([a, b, b], new Set(["file:///a.ts"]));

  assert.deepEqual(fresh, [b]);
});

test("a document is shown when an editor pane or the active editor holds it", () => {
  const visible = documentAt("file:///visible.ts");
  const active = documentAt("file:///active.ts");
  const loadedOnly = documentAt("file:///peeked.ts");
  const window = { activeTextEditor: editorFor(active), visibleTextEditors: [editorFor(visible)] };

  assert.equal(isShownDocument(visible, window), true);
  assert.equal(isShownDocument(active, window), true);
  assert.equal(isShownDocument(loadedOnly, window), false);
});
