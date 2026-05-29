import assert from "node:assert/strict";
import test from "node:test";
import { scriptRegionsForDocument } from "../../src/imports/scriptRegions.js";

test("scriptRegionsForDocument extracts Svelte TypeScript script content with absolute offset", () => {
  const source = [
    "<script lang=\"ts\">",
    "  import dayjs from 'dayjs';",
    "</script>",
    "<h1>{dayjs().format()}</h1>",
  ].join("\n");

  const regions = scriptRegionsForDocument("Component.svelte", source);

  assert.equal(regions.length, 1);
  assert.equal(regions[0]?.language, "ts");
  assert.equal(regions[0]?.runtime, "component");
  assert.equal(regions[0]?.source.trim(), "import dayjs from 'dayjs';");
  assert.equal(source.slice(regions[0]?.offset ?? -1).startsWith("\n  import dayjs"), true);
});

test("scriptRegionsForDocument extracts module and instance Svelte scripts", () => {
  const source = [
    "<script context=\"module\">",
    "  import { browser } from '$app/environment';",
    "</script>",
    "<script>",
    "  import dayjs from 'dayjs';",
    "</script>",
  ].join("\n");

  const regions = scriptRegionsForDocument("Component.svelte", source);

  assert.deepEqual(regions.map((region) => region.language), ["js", "js"]);
  assert.deepEqual(regions.map((region) => region.runtime), ["component", "component"]);
  assert.equal(regions.length, 2);
});

test("scriptRegionsForDocument extracts Astro frontmatter as server runtime", () => {
  const source = [
    "---",
    "import Icon from 'astro-icon';",
    "const title = 'Home';",
    "---",
    "<h1>{title}</h1>",
  ].join("\n");

  const regions = scriptRegionsForDocument("Page.astro", source);

  assert.equal(regions.length, 1);
  assert.equal(regions[0]?.language, "ts");
  assert.equal(regions[0]?.runtime, "server");
  assert.equal(regions[0]?.source.includes("import Icon from 'astro-icon';"), true);
  assert.equal(regions[0]?.offset, 4);
});

test("scriptRegionsForDocument extracts processed Astro client scripts", () => {
  const source = [
    "<h1>Demo</h1>",
    "<script>",
    "  import confetti from 'canvas-confetti';",
    "</script>",
    "<script is:inline>",
    "  import ignored from 'not-bundled';",
    "</script>",
  ].join("\n");

  const regions = scriptRegionsForDocument("Page.astro", source);

  assert.equal(regions.length, 1);
  assert.equal(regions[0]?.runtime, "client");
  assert.equal(regions[0]?.source.includes("canvas-confetti"), true);
});

test("scriptRegionsForDocument keeps plain JavaScript documents as a single region", () => {
  const source = "import dayjs from 'dayjs';";

  const regions = scriptRegionsForDocument("sample.ts", source);

  assert.deepEqual(regions, [{ filename: "sample.ts", source, offset: 0, language: "ts", runtime: "component" }]);
});
