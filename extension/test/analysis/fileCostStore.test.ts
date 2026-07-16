import assert from "node:assert/strict";
import test from "node:test";
import { DocumentAnalysisStates } from "../../src/analysis/documentStates.js";
import type { DocumentFileCost } from "../../src/analysis/fileSize.js";
import type { ImportAnalysisState } from "../../src/analysis/state.js";
import { detectedImport } from "../helpers/detectedImport.js";

const documentKey = "file:///c:/workspace/app/src/index.ts";

const loading = (specifier: string): ImportAnalysisState => ({
  detected: detectedImport({ specifier, packageName: specifier }),
  status: "loading",
});

const cost = (brotliBytes: number): DocumentFileCost => ({
  brotliBytes,
  error: null,
  diagnostics: [],
});

/**
 * The File Cost belongs to ONE version of ONE document: it is the daemon's combined build over the
 * imports that document had. The moment a new analysis opens, the number in hand describes the file
 * as it was before the user's last keystroke — the import they just deleted is still in it — and a
 * budget judged against it is judged against a document that no longer exists.
 */
test("a new analysis drops the previous File Cost", () => {
  const documents = new DocumentAnalysisStates();

  documents.set(documentKey, [loading("lodash-es")], 1);
  documents.setFileCost(documentKey, cost(55_000));
  assert.deepEqual(documents.fileCost(documentKey), cost(55_000));

  documents.set(documentKey, [loading("lodash-es")], 2);

  assert.equal(
    documents.fileCost(documentKey),
    undefined,
    "the file the last File Cost measured is not the file on screen any more",
  );
});

test("clearing a document drops its File Cost with its states", () => {
  const documents = new DocumentAnalysisStates();

  documents.set(documentKey, [loading("lodash-es")], 1);
  documents.setFileCost(documentKey, cost(55_000));
  documents.clear(documentKey);

  assert.equal(documents.fileCost(documentKey), undefined);
  assert.deepEqual(documents.get(documentKey), []);
});
