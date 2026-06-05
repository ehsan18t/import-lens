import assert from "node:assert/strict";
import test from "node:test";
import { substitutionSuggestionsFor } from "../../src/guidance/substitutions.js";

test("substitutionSuggestionsFor uses local curated mappings only", () => {
  assert.deepEqual(substitutionSuggestionsFor("moment").map((item) => item.packageName), ["dayjs", "date-fns"]);
  assert.deepEqual(substitutionSuggestionsFor("uuid/v4", "uuid").map((item) => item.packageName), ["uuid"]);
  assert.deepEqual(substitutionSuggestionsFor("unknown-lib"), []);
});
