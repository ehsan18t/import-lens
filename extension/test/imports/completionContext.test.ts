import assert from "node:assert/strict";
import test from "node:test";
import { namedImportCompletionContext } from "../../src/imports/completionContext.js";

test("namedImportCompletionContext returns specifier and imported names inside named import braces", () => {
  const source = `import { alpha, beta as renamed, type Gamma,  } from "tiny-lib";`;
  const offset = source.indexOf("type Gamma");

  assert.deepEqual(namedImportCompletionContext(source, offset), {
    specifier: "tiny-lib",
    importedNames: ["alpha", "beta", "Gamma"],
  });
});

test("namedImportCompletionContext supports multiline named imports", () => {
  const source = `import {\n  alpha,\n  beta,\n} from '@scope/pkg';`;
  const offset = source.indexOf("beta");

  assert.deepEqual(namedImportCompletionContext(source, offset), {
    specifier: "@scope/pkg",
    importedNames: ["alpha", "beta"],
  });
});

test("namedImportCompletionContext ignores positions outside named import braces", () => {
  const source = `import { alpha } from "tiny-lib";\nconsole.log(alpha);`;

  assert.equal(namedImportCompletionContext(source, source.indexOf("console")), null);
});

test("namedImportCompletionContext ignores import-like text in comments", () => {
  const source = `// import { alpha } from "tiny-lib";\nconst value = 1;`;

  assert.equal(namedImportCompletionContext(source, source.indexOf("alpha")), null);
});
