import assert from "node:assert/strict";
import test from "node:test";
import {
  packageJsonDependencyEntries,
  packageJsonDependencySections,
} from "../../src/guidance/packageJsonDependencies.js";

test("packageJsonDependencyEntries extracts dependency names, sections, and source ranges", () => {
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
      section: item.section,
      line: item.range.start.line,
      valueStart: item.valueRange.start.character,
    })),
    [
      { name: "react", version: "19.0.0", section: "dependencies", line: 2, valueStart: 13 },
      { name: "vitest", version: "4.0.0", section: "devDependencies", line: 5, valueStart: 14 },
    ],
  );
});

test("packageJsonDependencySections returns dependency block positions for summaries", () => {
  const source = [
    "{",
    "  \"dependencies\": {",
    "    \"react\": \"19.0.0\"",
    "  },",
    "  \"optionalDependencies\": {",
    "    \"fsevents\": \"2.3.3\"",
    "  }",
    "}",
  ].join("\n");

  assert.deepEqual(
    packageJsonDependencySections(source).map((section) => ({
      section: section.section,
      line: section.range.start.line,
      objectStart: section.objectRange.start.character,
    })),
    [
      { section: "dependencies", line: 1, objectStart: 18 },
      { section: "optionalDependencies", line: 4, objectStart: 26 },
    ],
  );
});
