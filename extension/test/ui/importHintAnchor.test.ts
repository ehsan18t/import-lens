import assert from "node:assert/strict";
import test from "node:test";
import {
  type ImportHintAnchorDocument,
  importHintAnchorPosition,
} from "../../src/ui/importHintAnchor.js";
import { detectedImport, sourceRange } from "../helpers/detectedImport.js";

test("importHintAnchorPosition anchors at the end of the import statement", () => {
  const lineText = 'import { ArrowDown } from "lucide-react";';
  const specifierEnd = lineText.lastIndexOf('"') + 1;
  const detected = detectedImport({
    specifierRange: sourceRange(0, 28, specifierEnd),
    statementRange: sourceRange(0, 0, lineText.length),
  });
  const document: ImportHintAnchorDocument = {
    lineCount: 1,
    lineAt: () => ({ text: lineText }),
  };

  const position = importHintAnchorPosition(document, detected);

  assert.equal(position.line, 0);
  assert.equal(position.character, lineText.length);
  assert.notEqual(position.character, specifierEnd);
});

test("importHintAnchorPosition clamps to the current line length", () => {
  const lineText = 'import x from "short"';
  const detected = detectedImport({
    specifierRange: sourceRange(0, 8, 30),
    statementRange: sourceRange(0, 0, 33),
  });
  const document: ImportHintAnchorDocument = {
    lineCount: 1,
    lineAt: () => ({ text: lineText }),
  };

  const position = importHintAnchorPosition(document, detected);

  assert.equal(position.character, lineText.length);
});
