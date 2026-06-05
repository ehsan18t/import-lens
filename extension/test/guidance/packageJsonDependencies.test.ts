import assert from "node:assert/strict";
import test from "node:test";
import { packageJsonDependencyEntries } from "../../src/guidance/packageJsonDependencies.js";

test("packageJsonDependencyEntries extracts dependency names and source ranges", () => {
  const source = [
    "{",
    "  \"dependencies\": {",
    "    \"react\": \"19.0.0\"",
    "  },",
    "  \"devDependencies\": {",
    "    \"vitest\": \"4.0.0\"",
    "  }",
    "}",
  ].join("\n");

  assert.deepEqual(
    packageJsonDependencyEntries(source).map((item) => ({
      name: item.name,
      version: item.version,
      line: item.range.start.line,
    })),
    [
      { name: "react", version: "19.0.0", line: 2 },
      { name: "vitest", version: "4.0.0", line: 5 },
    ],
  );
});
