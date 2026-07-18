#!/usr/bin/env node

// Fixture coverage, stated plainly so nobody mistakes a green run for more than it is:
//
//   synthetic (offline, deterministic)
//     - flat / branchy: tree-shaking behavior, incl. the only assertion that an
//       unreachable module is EXCLUDED from the breakdown.
//     - typescript package: the `graph.rs` TypeScript transform path, the only place
//       the daemon transforms real TS. A lowered `enum` and `namespace` both codegen
//       as IIFEs, so this doubles as coverage of the minifier's unused-IIFE analysis.
//     - emitted asset: JS imports CSS whose surviving `url()` references a local WOFF2;
//       esbuild and Import Lens must both emit/count that font exactly once. This reaches the
//       font INDIRECTLY, through CSS — it does not cover a binary imported from JavaScript.
//     - direct binary import: JS imports a `.wasm` AND a `.woff2` straight from source, the shape
//       where the daemon stubs the module to `ModuleType::Empty` at the `load` hook and counts the
//       file's raw bytes as a separate artifact. That stubbing deletes the URL reference code a
//       real file-loader build emits, so it is a MODEL, and this is the only place the model is
//       checked against an oracle rather than asserted. It is also the only benchmark whose
//       payload is incompressible, hence the only one whose brotli axis gates anything.
//   real packages (downloaded on demand, lockfile-pinned)
//     - css-tree: deep ESM graph with transitive dependencies.
//     - date-fns: deep zero-dependency ESM graph.
//     - lodash:   the CommonJS path -- `SourceType::cjs()` and the `;(() => {…})();`
//                 wrapper that `pipeline/cjs.rs` builds.
//     - refractor: a sideEffects glob anchored at the package root.
//     - @uiw/react-md-editor: the only real package whose published ESM entry actually
//                 does `import "./index.css"`, so it is the one benchmark that compares
//                 ASSET COUNTING (B2) against the oracle -- both sides must fold in the
//                 same stylesheet exactly once, which `minifiedTolerance` is what checks.
//
// NOT covered: the `.js`-containing-JSX retry path (`graph.rs`), which is a
// parse-failure fallback; the mangler's exported-destructuring handling, which
// `pipeline/bundle.rs` puts out of reach by stripping `export ` before minification;
// and -- despite what this file used to claim -- Lightning CSS's `@import` handling.
// The CSS fixture's whole reachable stylesheet graph contains ZERO `@import`
// statements (checked 2026-07-17): it aggregates CSS through JS `import "./x.css"`
// instead. So the `@import` tree walk, the cycle canonicalization and the synthetic
// entry -- the most intricate part of B2 -- are covered by the unit tests in
// `daemon/src/pipeline/assets.rs`, and by nothing here.
import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { copyFile, mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { brotliCompressSync, constants as zlibConstants } from "node:zlib";
import { decode, encode } from "@msgpack/msgpack";
import * as esbuild from "esbuild";

const protocolVersion = 6;
const packageName = "importlens-accuracy-fixture";
const typedPackageName = "importlens-accuracy-ts-fixture";
const assetPackageName = "importlens-accuracy-asset-fixture";
const binaryPackageName = "importlens-accuracy-binary-fixture";
const emittedFontBytes = 8 * 1024;
// The direct-import fixture's two binaries. Deliberately DIFFERENT sizes from each other and from
// `emittedFontBytes`, so a mixed-up artifact shows as a byte mismatch rather than a silent pass.
const directWasmBytes = 6 * 1024;
const directFontBytes = 10 * 1024;
// Tolerances for the direct-import benchmark. Both are far TIGHTER than the global 25%, which is
// the point: this is the only fixture with no compressor-gap noise to hide behind.
//
// Measured 2026-07-18. Import Lens 16446 B br / 16435 B minified; esbuild 16492 B br / 16480 B
// minified. Delta 46 B br (0.28%) and 45 B minified (0.27%) — we read LOW on both axes, by the same
// amount, and that amount is fully explained:
//
//   esbuild's JS chunk is 96 B and reads, in full:
//     var o="./probe-QSYIHYBW.wasm";var r="./probe-CZGQ6QWY.woff2";var e=()=>o+r;export{e as widget};
//   ours is 51 B, because the two modules are stubbed to `ModuleType::Empty` at the `load` hook.
//   96 - 51 = 45 B. The binaries themselves (6144 + 10240 = 16384 B) are counted IDENTICALLY by
//   both sides. So the neutral stubbing model agrees with the oracle to within the URL reference
//   code it declines to emit, and nothing else.
//
// Both axes read the same because the payload is incompressible by construction (see
// `incompressibleBytes`): it compresses to length+4 at quality 4 AND quality 11, so the q4-vs-q11
// asymmetry that inflates every other benchmark to 2.6-24.8% is absent here. That is why brotli can
// gate this fixture honestly when it cannot gate the CSS one.
//
// 1% on both: ~3.6x the honest 0.27%, so a codegen change that moved our stub chunk to zero bytes
// (0.58%) or up to esbuild's size (0.00%) stays green, while every failure that matters is orders
// of magnitude past it — dropping BOTH artifacts reads 99.7%, double-counting them 99%, and missing
// just the wasm 37.6%. The per-artifact `expectedAssets` check below is what pins the counts
// exactly; these two numbers gate the TOTAL. If either goes red, the shipping model changed. Do not
// raise them.
const binaryModelTolerances = { brotli: 0.01, minified: 0.01 };
// Maximum accepted brotli delta against the esbuild oracle, as a fraction.
//
// Derivation: the worst delta observed across the JavaScript benchmarks (2026-07-17
// re-baseline) is 15.0% (refractor); the rest sit at 2.6-13%. 25% leaves headroom
// over that for a legitimate compiler-stack bump before it turns CI red for a
// non-bug, while a real regression (a dangling binding dragging a dead module into
// the bundle) still moves the number far past it. The former 75% default could not
// fail on anything short of a catastrophe.
//
// Why every benchmark reads high at all: the daemon compresses brotli at quality 4
// (it runs per keystroke) and this oracle uses quality 11 (what a CDN serves), so a
// ~10-15% gap is baked in and says nothing about what was counted. The CSS benchmark
// carries its own tolerance because that same gap is amplified on highly-compressible
// stylesheets; see its entry in `realFixtures`.
//
// Re-derive this the next time the observed worst case moves: keep it at roughly
// twice the worst accepted delta, and never raise it to make a red run green without
// first proving the delta is a codegen difference, not a bug.
const tolerance = Number(process.env.IMPORT_LENS_ACCURACY_TOLERANCE ?? "0.25");
// Local runs may be offline; CI and upgrade baselines must never silently measure
// nothing. `validate.yml` sets this, and so must any pre/post-upgrade baseline run.
const requireFixtures = process.env.IMPORT_LENS_ACCURACY_REQUIRE_FIXTURES === "1";
const fixturesDir = fileURLToPath(new URL("accuracy-fixtures/", import.meta.url));
const installTimeoutMs = 300_000;

// Real-world packages, each present for one reason. `named` is the export whose
// import cost we measure. Versions come from the fixture manifest so there is a
// single source of truth for them.
const realFixtures = [
  { package: "css-tree", named: "parse", label: "css-tree (deep ESM graph, transitive deps)" },
  { package: "date-fns", named: "format", label: "date-fns (deep zero-dependency ESM graph)" },
  { package: "lodash", named: "debounce", label: "lodash (CommonJS wrapper path)" },
  {
    package: "refractor",
    named: "refractor",
    label: "refractor (sideEffects glob anchored at the package root)",
  },
  // The ONLY real package in the set whose published ESM entry actually does `import "./index.css"`
  // — react-toastify, react-datepicker, swiper and react-loading-skeleton all *ship* a stylesheet
  // but none imports one from published JavaScript, so none would exercise this at all. It is what
  // gates asset counting (B2) against the oracle: both sides must fold in the same stylesheet
  // exactly once, so a double count or a dropped stylesheet shows up here as a delta rather than as
  // a wrong number in the product. Its own stylesheets contain no `@import`, so that part of B2 is
  // the unit tests' job, not this benchmark's.
  {
    package: "@uiw/react-md-editor",
    named: "headingExecute",
    label: "@uiw/react-md-editor (ESM entry imports CSS: asset counting vs the oracle)",
    // Its own tolerances, because the global one cannot gate this benchmark honestly.
    //
    // The daemon compresses brotli at quality 4 (it runs per keystroke) while this oracle uses
    // quality 11 (what a CDN actually serves), so EVERY benchmark reads high for a reason that has
    // nothing to do with what was counted: 2.6-15% across the JS set. CSS compresses far better
    // than JS, so that same asymmetry was amplified here to 24.8% at brotli quality 4 — nearly the
    // entire 25% budget — which is why this fixture used to carry its own 35% brotli tolerance.
    //
    // Quality 9 (2026-07-18) halved that gap: this benchmark now reads 8.8% on brotli, comfortably
    // inside the shared gate, so the override is gone. That is the point worth keeping: the 35% was
    // never measuring the stylesheet, it was measuring OUR compressor being weaker than the oracle,
    // and a tolerance that wide gated nothing. Measured 2026-07-17 by holding CSS at q4 and varying
    // only the fold count: ZERO folds read 22.7%, ONCE 24.8%, TWICE 26.9% — all three under 35%, so
    // the fixture would have stayed green with asset counting deleted outright.
    //
    // The MINIFIED pair is where the signal is, because neither side compresses it and the CSS is
    // ~3% of that total rather than ~1.7%. Measured the same way: fold ZERO reads 3.80%, ONCE
    // 0.81%, TWICE 2.19%.
    //
    // 1.5% sits between those, deliberately: it is nearly 2x the honest reading and a third below a
    // double count, so both failure directions are caught with room, and it is what makes this
    // fixture gate B2 at all. (2% would also catch a double count, but by under 10% — thin enough
    // that a small real drift could hide one.) Our JS reads 0.8% LOW against esbuild minifier,
    // which is why the band is not centred on zero. Both minifiers are exact-pinned and
    // upgrade-gated, so this only moves on a deliberate re-baseline. If it goes red, a stylesheet
    // fold count changed; do not raise the number.
    minifiedTolerance: 0.015,
    expectsStylesheet: true,
  },
];

const main = async () => {
  const workspace = await mkdtemp(path.join(os.tmpdir(), "importlens-accuracy-"));
  let daemon;

  try {
    // Install first: `pnpm` owns `node_modules`, and the synthetic fixture is written
    // into it afterwards so the install cannot prune it.
    const realFixtureState = await installRealFixtures(workspace);
    const fixture = await writeFixture(workspace);
    const benchmarks = [
      {
        label: "same-module unused export",
        activeDocumentPath: fixture.flatActiveDocumentPath,
        package: packageName,
        version: "1.0.0",
        named: "light",
      },
      {
        label: "branchy unused export dependency",
        activeDocumentPath: fixture.branchyActiveDocumentPath,
        package: packageName,
        version: "1.0.0",
        named: "used",
        excludedModule: "/huge.js",
      },
      {
        label: "typescript enum and namespace transform",
        activeDocumentPath: fixture.typedActiveDocumentPath,
        package: typedPackageName,
        version: "1.0.0",
        named: "typed",
      },
      {
        label: "CSS local font emission",
        activeDocumentPath: fixture.assetActiveDocumentPath,
        package: assetPackageName,
        version: "1.0.0",
        named: "widget",
        expectsStylesheet: true,
        expectedAssets: [{ extension: ".woff2", kind: "font", bytes: emittedFontBytes }],
        minifiedTolerance: 0.02,
      },
      {
        label: "direct JS import of wasm and font",
        activeDocumentPath: fixture.binaryActiveDocumentPath,
        package: binaryPackageName,
        version: "1.0.0",
        named: "widget",
        expectedAssets: [
          { extension: ".wasm", kind: "wasm", bytes: directWasmBytes },
          { extension: ".woff2", kind: "font", bytes: directFontBytes },
        ],
        // See the measurement note above `binaryModelTolerances`.
        tolerance: binaryModelTolerances.brotli,
        minifiedTolerance: binaryModelTolerances.minified,
      },
      ...(realFixtureState.installed
        ? await writeRealFixtureEntries(workspace, realFixtureState.versions)
        : []),
    ];

    daemon = await startDaemon(workspace);

    for (const [index, benchmark] of benchmarks.entries()) {
      const importLens = await importLensNamedSize(daemon, workspace, benchmark, index + 1);
      const esbuildSize = await esbuildNamedSize(
        workspace,
        benchmark.activeDocumentPath,
        benchmark.expectsStylesheet ?? false,
        benchmark.expectedAssets,
      );
      const delta = Math.abs(importLens.brotliBytes - esbuildSize.brotliBytes);
      const relativeDelta = delta / Math.max(esbuildSize.brotliBytes, 1);
      const minifiedDelta = Math.abs(importLens.minifiedBytes - esbuildSize.minifiedBytes);
      const relativeMinifiedDelta = minifiedDelta / Math.max(esbuildSize.minifiedBytes, 1);

      process.stdout.write(
        [
          `${benchmark.label}:`,
          `  Import Lens named import: ${importLens.brotliBytes} B br (${importLens.minifiedBytes} B minified)`,
          `  esbuild named import: ${esbuildSize.brotliBytes} B br (${esbuildSize.minifiedBytes} B minified)`,
          `  relative delta: ${(relativeDelta * 100).toFixed(1)}% br, ${(relativeMinifiedDelta * 100).toFixed(2)}% minified (${minifiedDelta} B)`,
        ].join("\n"),
      );
      process.stdout.write("\n");

      // A benchmark may carry its own tolerance where the global one cannot gate it honestly; see
      // the CSS fixture. Everything else is held to the global number.
      const benchmarkTolerance = benchmark.tolerance ?? tolerance;

      if (relativeDelta > benchmarkTolerance) {
        throw new Error(
          `${benchmark.label} accuracy delta ${(relativeDelta * 100).toFixed(1)}% exceeds ${(benchmarkTolerance * 100).toFixed(1)}% tolerance`,
        );
      }

      // The brotli delta above cannot gate WHAT WAS COUNTED on a benchmark whose assets are a small
      // share of a compressed total, because both sides' compressors disagree by far more than the
      // assets weigh (see the CSS fixture). Where a benchmark says so, hold the MINIFIED totals too:
      // neither side compresses them, so the compressor gap is absent and the only thing left to
      // explain a delta is a difference in what got folded in.
      if (benchmark.minifiedTolerance !== undefined) {
        if (relativeMinifiedDelta > benchmark.minifiedTolerance) {
          throw new Error(
            `${benchmark.label} minified delta ${(relativeMinifiedDelta * 100).toFixed(2)}% exceeds ${(benchmark.minifiedTolerance * 100).toFixed(2)}% tolerance. This is the axis that gates what was COUNTED: a stylesheet folded in twice, or not at all, moves it. Do not raise this number to make it green.`,
          );
        }
      }

      // Exactness, per expected artifact: EXACTLY ONE contribution of that kind, at exactly that
      // byte count. `find` alone would have accepted a second, duplicate contribution of the same
      // kind, so the count is asserted rather than the first match.
      for (const expected of benchmark.expectedAssets ?? []) {
        const contributions = importLens.assetBreakdown.filter(
          (asset) => asset.kind === expected.kind,
        );
        if (contributions.length !== 1 || contributions[0].raw_bytes !== expected.bytes) {
          throw new Error(
            `${benchmark.label} expected exactly one ${expected.bytes}-byte ${expected.kind} ` +
              `contribution, got ${JSON.stringify(contributions)}`,
          );
        }
      }

      if (
        benchmark.excludedModule &&
        importLens.moduleBreakdown.some((module) =>
          module.path.replaceAll("\\", "/").endsWith(benchmark.excludedModule),
        )
      ) {
        throw new Error(`${benchmark.label} unexpectedly included ${benchmark.excludedModule}`);
      }
    }

    if (!realFixtureState.installed) {
      warnRealFixturesSkipped(realFixtureState.reason);
    }
  } finally {
    await daemon?.shutdown();
    await rm(workspace, { recursive: true, force: true });
  }
};

// Copy the pinned manifest plus its lockfile into the workspace and install them.
// `--frozen-lockfile` is what makes the byte counts reproducible: css-tree depends
// on source-map-js through a caret range, so exact direct versions alone would let
// a transitive patch move the numbers we diff across an upgrade.
const installRealFixtures = async (workspace) => {
  try {
    await copyFile(path.join(fixturesDir, "package.json"), path.join(workspace, "package.json"));
    await copyFile(
      path.join(fixturesDir, "pnpm-lock.yaml"),
      path.join(workspace, "pnpm-lock.yaml"),
    );
    await writeFile(path.join(workspace, ".npmrc"), fixtureNpmrc(), "utf8");
    await runPnpmInstall(workspace);
  } catch (error) {
    const reason = error instanceof Error ? error.message : String(error);
    if (requireFixtures) {
      throw new Error(`real accuracy fixtures are required but could not be installed: ${reason}`);
    }
    return { installed: false, reason };
  }

  // The install succeeded, so a precondition failure here is a broken fixture, not an
  // offline laptop. It must never degrade to a skip, in either mode.
  return { installed: true, versions: await assertRealFixturePreconditions(workspace) };
};

// `--frozen-lockfile` refuses to run when a setting recorded in the lockfile disagrees
// with the effective pnpm config (ERR_PNPM_LOCKFILE_CONFIG_MISMATCH). Those settings
// would otherwise come from the machine's global pnpm config, so a developer with
// `auto-install-peers=false` could not install the fixtures at all. Pin every setting
// the lockfile records, and keep this in step with `accuracy-fixtures/pnpm-lock.yaml`.
const fixtureNpmrc = () =>
  [
    // The hoisted linker gives a real `node_modules/<pkg>` tree, which is what
    // `find_package_root` walks and what a user's project actually looks like. It also
    // sidesteps junction canonicalization on Windows. Not a lockfile-validated setting.
    "node-linker=hoisted",
    "auto-install-peers=true",
    "exclude-links-from-lockfile=false",
    "",
  ].join("\n");

const runPnpmInstall = (cwd) =>
  new Promise((resolve, reject) => {
    const args = [
      "install",
      "--frozen-lockfile",
      "--ignore-workspace",
      "--ignore-scripts",
      "--prefer-offline",
    ];
    // On Windows `pnpm` resolves to `pnpm.CMD`, which CreateProcess cannot launch
    // directly; run it through the shell, mirroring `update-compiler-stack.mjs`.
    const child =
      process.platform === "win32"
        ? spawn(`pnpm ${args.join(" ")}`, { cwd, shell: true, stdio: ["ignore", "ignore", "pipe"] })
        : spawn("pnpm", args, { cwd, stdio: ["ignore", "ignore", "pipe"] });

    const stderr = [];
    child.stderr.on("data", (chunk) => stderr.push(chunk.toString()));

    // Bound the install the way the daemon connect and request paths are bounded. A
    // wedged pnpm would otherwise hang until the CI job's own timeout, with no clue why.
    const timer = setTimeout(() => {
      child.kill();
      reject(new Error(`pnpm install timed out after ${installTimeoutMs}ms`));
    }, installTimeoutMs);

    child.once("error", (error) => {
      clearTimeout(timer);
      reject(error);
    });
    child.once("close", (code) => {
      clearTimeout(timer);
      if (code === 0) {
        resolve();
        return;
      }
      reject(new Error(`pnpm install exited with code ${code}: ${stderr.join("").trim()}`));
    });
  });

// Guard the properties each fixture was chosen for. Without these a future version
// bump could silently stop exercising the path the benchmark exists to cover, and
// the suite would keep reporting green.
const assertRealFixturePreconditions = async (workspace) => {
  const versions = {};
  const manifests = {};

  for (const fixture of realFixtures) {
    const manifestPath = path.join(workspace, "node_modules", fixture.package, "package.json");
    try {
      manifests[fixture.package] = JSON.parse(await readFile(manifestPath, "utf8"));
    } catch (error) {
      throw new Error(
        `accuracy fixture ${fixture.package} is missing after a frozen install; ` +
          `expected ${manifestPath} (${error instanceof Error ? error.message : String(error)})`,
      );
    }
    versions[fixture.package] = manifests[fixture.package].version;
  }

  if (manifests.lodash.module !== undefined) {
    throw new Error(
      "lodash fixture declares a `module` field, so it now resolves to an ESM entry; " +
        "the CommonJS wrapper path is no longer covered by any benchmark",
    );
  }

  // refractor is here for ONE property: its `sideEffects` carries a pattern with a `/`.
  //
  // That is the branch nothing else in this suite reaches. `sideEffects` globs are matched against
  // the entry's PACKAGE-RELATIVE path, and the matcher prefixes `**/` to any pattern that has no
  // separator (or a `./` prefix) — so those patterns match an absolute path too, by accident, and
  // stay green even when the path being matched is wrong. Only a pattern that CONTAINS a `/` is
  // used verbatim and anchored at the package root, and it is the one that goes red.
  //
  // It went red for real: the daemon handed Rolldown a `\\?\` verbatim entry id against a
  // non-canonical package.json path, the relativization silently degraded to the whole absolute
  // path, `["lib/all.js","lib/common.js"]` matched nothing, and refractor's ~35 gated
  // `refractor.register(lang)` statements were tree-shaken away — 30,229 B reported for a package
  // esbuild puts at 114,296 B. Every offline test passed. THIS suite, comparing real bytes against
  // an independent bundler, is what a wrong number on a real package has to answer to.
  //
  // So the property is guarded rather than assumed: if a future version drops the anchored pattern,
  // or moves the entry out from under it, this benchmark silently stops covering that branch.
  const sideEffects = manifests.refractor.sideEffects;
  const anchored =
    Array.isArray(sideEffects) &&
    sideEffects.some((pattern) => typeof pattern === "string" && pattern.includes("/"));
  if (!anchored) {
    throw new Error(
      "refractor fixture no longer declares a `sideEffects` pattern containing a `/` " +
        `(got ${JSON.stringify(sideEffects)}); no benchmark now covers a package-root-anchored ` +
        "sideEffects glob, which is the form that hid a 3.7x undercount",
    );
  }

  // @uiw/react-md-editor is here for ONE property: its published ESM entry really does
  // `import "./index.css"`. That is the whole reason it is the only real package in this suite that
  // exercises asset counting (B2) against an independent bundler — react-toastify, react-datepicker,
  // swiper and react-loading-skeleton all SHIP a stylesheet but none imports one from published
  // JavaScript, so none would exercise it at all.
  //
  // If a future version drops that import, both sides fold in no CSS, every delta stays inside
  // tolerance, and this benchmark silently degrades into a second pure-JS one while still claiming
  // in its label to gate asset counting. That is the exact shape the refractor guard above exists to
  // prevent, so it is guarded rather than assumed.
  const editorPackage = "@uiw/react-md-editor";
  const editorManifest = manifests[editorPackage];
  const editorEntry =
    editorManifest.exports?.["."]?.import ?? editorManifest.module ?? editorManifest.main;
  const editorEntrySource = await readFile(
    path.join(workspace, "node_modules", editorPackage, editorEntry),
    "utf8",
  );
  if (!/import\s*["'][^"']+\.css["']/u.test(editorEntrySource)) {
    throw new Error(
      `${editorPackage} fixture's ESM entry (${editorEntry}) no longer imports a stylesheet; ` +
        "no benchmark now compares asset counting against the oracle, and the CSS fixture has " +
        "quietly become a duplicate JS one",
    );
  }

  return versions;
};

const writeRealFixtureEntries = async (workspace, versions) => {
  const sourceRoot = path.join(workspace, "src");
  const benchmarks = [];

  for (const fixture of realFixtures) {
    // A SCOPED package name carries a slash (`@uiw/react-md-editor`), which would turn this
    // filename into a nested path whose directory does not exist. The entry file's name is
    // arbitrary; only the specifier inside it matters.
    const entryName = fixture.package.replaceAll(/[^a-z0-9.-]/giu, "-");
    const activeDocumentPath = path.join(sourceRoot, `real-${entryName}-entry.js`);
    await writeFile(
      activeDocumentPath,
      `export { ${fixture.named} } from "${fixture.package}";\n`,
      "utf8",
    );
    benchmarks.push({
      label: fixture.label,
      activeDocumentPath,
      package: fixture.package,
      version: versions[fixture.package],
      named: fixture.named,
      tolerance: fixture.tolerance,
      minifiedTolerance: fixture.minifiedTolerance,
      expectsStylesheet: fixture.expectsStylesheet,
    });
  }

  return benchmarks;
};

const warnRealFixturesSkipped = (reason) => {
  const rule = "!".repeat(78);
  process.stderr.write(
    [
      "",
      rule,
      "!! REAL-PACKAGE BENCHMARKS DID NOT RUN. THIS RUN MEASURED LESS THAN IT LOOKS.",
      `!! reason: ${reason}`,
      ...realFixtures.map((fixture) => `!! skipped: ${fixture.label}`),
      "!!",
      "!! The synthetic benchmarks above still ran, but nothing here exercised the",
      "!! CommonJS path or any real-world module graph. Do NOT use this run as an",
      "!! OXC upgrade baseline. Set IMPORT_LENS_ACCURACY_REQUIRE_FIXTURES=1 to turn",
      "!! a failed fixture install into a hard error.",
      rule,
      "",
    ].join("\n"),
  );
};

const writeFixture = async (workspace) => {
  const packageRoot = path.join(workspace, "node_modules", packageName);
  const sourceRoot = path.join(workspace, "src");
  await mkdir(packageRoot, { recursive: true });
  await mkdir(sourceRoot, { recursive: true });

  await writeFile(
    path.join(packageRoot, "package.json"),
    JSON.stringify(
      {
        name: packageName,
        version: "1.0.0",
        type: "module",
        module: "index.js",
        sideEffects: false,
      },
      null,
      2,
    ),
    "utf8",
  );
  await writeFile(
    path.join(packageRoot, "index.js"),
    [
      `import { small } from "./small.js";`,
      `import { huge } from "./huge.js";`,
      `export const light = ${JSON.stringify(deterministicPayload(12_000))};`,
      `export const unusedFlat = ${JSON.stringify(deterministicPayload(120_000))};`,
      `export const used = small;`,
      `export const unusedBranch = huge;`,
    ].join("\n"),
    "utf8",
  );
  await writeFile(
    path.join(packageRoot, "small.js"),
    `export const small = ${JSON.stringify(deterministicPayload(12_000))};\n`,
    "utf8",
  );
  await writeFile(
    path.join(packageRoot, "huge.js"),
    `export const huge = ${JSON.stringify(deterministicPayload(180_000))};\n`,
    "utf8",
  );

  const flatActiveDocumentPath = path.join(sourceRoot, "flat-entry.js");
  const branchyActiveDocumentPath = path.join(sourceRoot, "branchy-entry.js");
  await writeFile(flatActiveDocumentPath, `export { light } from "${packageName}";\n`, "utf8");
  await writeFile(branchyActiveDocumentPath, `export { used } from "${packageName}";\n`, "utf8");

  const typedActiveDocumentPath = await writeTypedFixture(workspace, sourceRoot);
  const assetActiveDocumentPath = await writeAssetFixture(workspace, sourceRoot);
  const binaryActiveDocumentPath = await writeBinaryFixture(workspace, sourceRoot);
  return {
    flatActiveDocumentPath,
    branchyActiveDocumentPath,
    typedActiveDocumentPath,
    assetActiveDocumentPath,
    binaryActiveDocumentPath,
  };
};

// The DIRECT-import binary fixture, and the only place the shipping model for a wasm/font imported
// straight from JavaScript is compared against an oracle.
//
// `writeAssetFixture` reaches its font INDIRECTLY, through a surviving CSS `url()`. That never
// exercises the other half of B2: the daemon intercepts a directly imported wasm/font at Rolldown's
// `load` hook, stubs the module to `ModuleType::Empty`, and counts the file's raw bytes as a
// separate artifact. Stubbing DELETES the reference code a real file-loader build emits, so the two
// models are not obviously the same thing, and until this fixture existed nobody had measured
// whether they agree. esbuild's `file` loader is the oracle: it emits the binary AND a JS module
// exporting its URL, which is precisely the difference this benchmark quantifies.
//
// Both binaries must stay REFERENCED (`widget` reads both bindings) or tree shaking drops the
// imports on both sides and the fixture measures nothing.
const writeBinaryFixture = async (workspace, sourceRoot) => {
  const packageRoot = path.join(workspace, "node_modules", binaryPackageName);
  await mkdir(packageRoot, { recursive: true });
  await writeFile(
    path.join(packageRoot, "package.json"),
    JSON.stringify(
      {
        name: binaryPackageName,
        version: "1.0.0",
        type: "module",
        module: "index.js",
        sideEffects: false,
      },
      null,
      2,
    ),
    "utf8",
  );
  await writeFile(
    path.join(packageRoot, "index.js"),
    [
      `import wasmUrl from "./probe.wasm";`,
      `import fontUrl from "./probe.woff2";`,
      `export const widget = () => wasmUrl + fontUrl;`,
      ``,
    ].join("\n"),
    "utf8",
  );
  await writeFile(
    path.join(packageRoot, "probe.wasm"),
    incompressibleBytes(directWasmBytes, 0x12345678),
  );
  await writeFile(
    path.join(packageRoot, "probe.woff2"),
    incompressibleBytes(directFontBytes, 0x6d2b79f5),
  );

  const activeDocumentPath = path.join(sourceRoot, "binary-entry.js");
  await writeFile(activeDocumentPath, `export { widget } from "${binaryPackageName}";\n`, "utf8");
  return activeDocumentPath;
};

const writeAssetFixture = async (workspace, sourceRoot) => {
  const packageRoot = path.join(workspace, "node_modules", assetPackageName);
  await mkdir(packageRoot, { recursive: true });
  await writeFile(
    path.join(packageRoot, "package.json"),
    JSON.stringify(
      {
        name: assetPackageName,
        version: "1.0.0",
        type: "module",
        module: "index.js",
        sideEffects: ["*.css"],
      },
      null,
      2,
    ),
    "utf8",
  );
  await writeFile(
    path.join(packageRoot, "index.js"),
    `import "./styles.css";\nexport const widget = "widget";\n`,
    "utf8",
  );
  await writeFile(
    path.join(packageRoot, "styles.css"),
    `@font-face { font-family: Probe; src: url("./probe.woff2") format("woff2"); }\n` +
      `.widget { font-family: Probe; }\n`,
    "utf8",
  );
  await writeFile(path.join(packageRoot, "probe.woff2"), deterministicBytes(emittedFontBytes));

  const activeDocumentPath = path.join(sourceRoot, "asset-entry.js");
  await writeFile(activeDocumentPath, `export { widget } from "${assetPackageName}";\n`, "utf8");
  return activeDocumentPath;
};

// The only TypeScript in the suite, and therefore the only thing that reaches the
// `graph.rs` transform. A lowered `enum` and `namespace` each codegen as an IIFE,
// so this also exercises the minifier's unused-IIFE analysis.
//
// It gets its own package, and the package entry *is* the TypeScript module, so the
// benchmark measures the transform and nothing else. Hanging it off the shared
// fixture's index.js instead made the entry re-export it indirectly, which drags
// that module's unrelated static imports into the bundle and buried the signal under
// 180 KB of unrelated payload.
//
// `Level` and `Meta` are read *dynamically* on purpose. `Level.High` alone folds to
// a constant and the enum object is then dead-code-eliminated, which would quietly
// delete the coverage this fixture exists to provide.
const writeTypedFixture = async (workspace, sourceRoot) => {
  const packageRoot = path.join(workspace, "node_modules", typedPackageName);
  await mkdir(packageRoot, { recursive: true });

  await writeFile(
    path.join(packageRoot, "package.json"),
    JSON.stringify(
      {
        name: typedPackageName,
        version: "1.0.0",
        type: "module",
        module: "index.ts",
        sideEffects: false,
      },
      null,
      2,
    ),
    "utf8",
  );
  await writeFile(
    path.join(packageRoot, "index.ts"),
    [
      `export enum Level {`,
      `  Low = 0,`,
      `  High = 1,`,
      `}`,
      ``,
      `export namespace Meta {`,
      `  export const label = "typed";`,
      `  export const width = 3;`,
      `}`,
      ``,
      `export interface Shape {`,
      `  kind: string;`,
      `  size: number;`,
      `  body: string;`,
      `}`,
      ``,
      `export const typed: Shape = {`,
      `  kind: Level[Level.High],`,
      `  size: Object.keys(Meta).length,`,
      `  body: ${JSON.stringify(deterministicPayload(12_000))},`,
      `};`,
      ``,
    ].join("\n"),
    "utf8",
  );

  const typedActiveDocumentPath = path.join(sourceRoot, "typed-entry.js");
  await writeFile(
    typedActiveDocumentPath,
    `export { typed } from "${typedPackageName}";\n`,
    "utf8",
  );
  return typedActiveDocumentPath;
};

const importLensNamedSize = async (daemon, workspace, benchmark, requestId) => {
  const response = await daemon.request({
    version: protocolVersion,
    request_id: requestId,
    workspace_root: workspace,
    active_document_path: benchmark.activeDocumentPath,
    imports: [
      {
        specifier: benchmark.package,
        package: benchmark.package,
        version: benchmark.version,
        named: [benchmark.named],
        import_kind: "named",
        runtime: "component",
      },
    ],
  });
  const result = response.imports?.[0];

  if (!result || result.error) {
    throw new Error(
      `Import Lens accuracy request failed for ${benchmark.label}: ${result?.error ?? "missing result"}`,
    );
  }

  return {
    brotliBytes: result.brotli_bytes,
    minifiedBytes: result.minified_bytes,
    moduleBreakdown: result.module_breakdown ?? [],
    assetBreakdown: result.asset_breakdown ?? [],
  };
};

const esbuildNamedSize = async (
  workspace,
  activeDocumentPath,
  expectsStylesheet = false,
  expectedAssets,
) => {
  const result = await esbuild.build({
    absWorkingDir: workspace,
    entryPoints: [activeDocumentPath],
    bundle: true,
    minify: true,
    write: false,
    // Required, not cosmetic: esbuild REFUSES to bundle a graph that imports CSS without an output
    // path ("Cannot import ... into a JavaScript file without an output path configured"), and
    // without one even a pure-JS build names its output `<stdout>` rather than `*.js`, so there is
    // nothing to classify. With it, one entry yields `entry.js` plus a sibling `entry.css`. Nothing
    // is written to disk (`write: false`); this only names the outputs, so no byte count moves.
    outdir: path.join(workspace, "esbuild-out"),
    format: "esm",
    platform: "browser",
    treeShaking: true,
    // `file` is the loader a real app configures for a binary it imports for its URL, and it is
    // what makes the direct-import fixture measurable at all: esbuild has NO default loader for
    // `.wasm`, so `import u from "./x.wasm"` is a hard build error without this line. Under `file`
    // esbuild emits the binary as its own artifact and a JS module exporting the URL — the JS half
    // is exactly what the daemon stubs away, which is the difference that benchmark quantifies.
    loader: { ".woff2": "file", ".wasm": "file" },
    logLevel: "silent",
  });

  // When the bundled graph imports CSS, esbuild gathers it into that sibling `.css`, so
  // `outputFiles` holds more than one entry and the JS is not guaranteed to be at index 0. The
  // daemon counts those stylesheet bytes now (B2), so the oracle must too, or the two would be
  // measuring different things and the comparison would be meaningless.
  //
  // Classify by extension and compress each artifact ON ITS OWN before summing — never concatenate
  // first — because that is exactly what the daemon does (ADR-0005: they are separate files that
  // ship separately). `reduce` over an empty CSS list is zero, so a pure-JS benchmark is unchanged.
  const stylesheets = result.outputFiles.filter((file) => file.path.endsWith(".css"));
  const javascript = result.outputFiles.filter((file) => file.path.endsWith(".js"));
  const emittedAssets = result.outputFiles.filter(
    (file) => !file.path.endsWith(".css") && !file.path.endsWith(".js"),
  );

  if (javascript.length === 0) {
    throw new Error("esbuild did not produce a JavaScript output file");
  }

  // The other half of the CSS fixture's precondition: the entry importing a stylesheet is what makes
  // the ORACLE emit one. If it ever stops, both sides fold in no CSS, the deltas stay green, and the
  // benchmark compares JS to JS while claiming to gate asset counting.
  if (expectsStylesheet && stylesheets.length === 0) {
    throw new Error(
      "esbuild emitted no stylesheet for a benchmark whose entire purpose is comparing counted " +
        "CSS against the oracle; the fixture is no longer exercising asset counting",
    );
  }

  // One benchmark may expect SEVERAL emitted artifacts (the direct-import fixture imports both a
  // wasm and a font), so this is a list. Each entry still demands exactly one match at exactly its
  // byte count — a list did not loosen the check, it just repeated it per extension.
  for (const expected of expectedAssets ?? []) {
    const matching = emittedAssets.filter((file) => file.path.endsWith(expected.extension));
    if (matching.length !== 1 || matching[0].contents.length !== expected.bytes) {
      throw new Error(
        `esbuild must emit one ${expected.bytes}-byte ${expected.extension} artifact; ` +
          `got ${matching.map((file) => `${file.path}:${file.contents.length}`).join(", ") || "none"}`,
      );
    }
  }

  const bytesOf = (files) => files.reduce((bytes, file) => bytes + file.contents.length, 0);
  const brotliBytesOf = (files) =>
    files.reduce((bytes, file) => bytes + brotliSize(file.contents), 0);

  return {
    brotliBytes:
      brotliBytesOf(javascript) + brotliBytesOf(stylesheets) + brotliBytesOf(emittedAssets),
    minifiedBytes: bytesOf(javascript) + bytesOf(stylesheets) + bytesOf(emittedAssets),
  };
};

const startDaemon = async (workspace) => {
  const storagePath = path.join(workspace, ".importlens-cache");
  await mkdir(storagePath, { recursive: true });
  const pipeName =
    process.platform === "win32"
      ? `\\\\.\\pipe\\import-lens-accuracy-${process.pid}-${randomUUID()}`
      : path.join(os.tmpdir(), `import-lens-accuracy-${process.pid}-${randomUUID()}.sock`);
  const child = spawn(
    "cargo",
    [
      "run",
      "--quiet",
      "--locked",
      "--bin",
      "import-lens-daemon",
      "--",
      "--pipe",
      pipeName,
      "--workspace",
      workspace,
      "--storage",
      storagePath,
    ],
    {
      cwd: fileURLToPath(new URL("..", import.meta.url)),
      stdio: ["ignore", "ignore", "pipe"],
    },
  );
  const stderr = [];
  child.stderr.on("data", (chunk) => stderr.push(chunk.toString()));

  try {
    const socket = await connectWithRetry(pipeName, 60_000, child, stderr);
    const client = daemonClient(socket);
    client.send({
      type: "hello",
      version: protocolVersion,
      workspace_root: workspace,
      storage_path: storagePath,
      enable_disk_cache: false,
      cache_max_size_mb: 512,
      // Deprecated and ignored by the daemon; kept for Hello frame compatibility.
      cache_max_age_days: 30,
      log_level: "warn",
    });

    return {
      request: client.request,
      shutdown: async () => {
        try {
          client.send({ type: "shutdown" });
        } catch {
          // best effort shutdown
        }
        socket.destroy();
        if (child.exitCode === null && child.signalCode === null) {
          child.kill();
        }
      },
    };
  } catch (error) {
    if (child.exitCode === null && child.signalCode === null) {
      child.kill();
    }
    throw error;
  }
};

const connectWithRetry = async (pipeName, timeoutMs, child, stderr) => {
  const started = Date.now();
  let lastError;

  while (Date.now() - started < timeoutMs) {
    if (child.exitCode !== null || child.signalCode !== null) {
      throw new Error(`daemon exited before IPC connection: ${stderr.join("").trim()}`);
    }

    try {
      return await new Promise((resolve, reject) => {
        const socket = net.createConnection(pipeName);
        socket.once("connect", () => resolve(socket));
        socket.once("error", reject);
      });
    } catch (error) {
      lastError = error;
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
  }

  const stderrText = stderr.join("").trim();
  const suffix = stderrText ? `; daemon stderr: ${stderrText}` : "";
  throw new Error(
    `failed to connect to Import Lens daemon: ${lastError?.message ?? "timeout"}${suffix}`,
  );
};

const daemonClient = (socket, { requestTimeoutMs = 60000 } = {}) => {
  // Responses are correlated positionally (this harness sends one request at a
  // time in order), so close/timeout/parse failures must reject the stragglers
  // rather than leave their promises pending forever.
  const pending = [];
  let buffer = Buffer.alloc(0);

  const remove = (item) => {
    const index = pending.indexOf(item);
    if (index !== -1) {
      pending.splice(index, 1);
    }
  };

  const settle = (item, apply) => {
    if (item.settled) {
      return;
    }
    item.settled = true;
    clearTimeout(item.timer);
    remove(item);
    apply();
  };

  const rejectAll = (error) => {
    for (const item of [...pending]) {
      settle(item, () => item.reject(error));
    }
  };

  socket.on("data", (chunk) => {
    buffer = Buffer.concat([buffer, chunk]);

    while (buffer.length >= 4) {
      const length = buffer.readUInt32BE(0);
      if (buffer.length < length + 4) {
        return;
      }

      const payload = buffer.subarray(4, 4 + length);
      buffer = buffer.subarray(4 + length);

      let message;
      try {
        message = decode(payload);
      } catch (error) {
        rejectAll(error instanceof Error ? error : new Error(String(error)));
        socket.destroy();
        return;
      }

      const item = pending[0];
      if (item) {
        settle(item, () => item.resolve(message));
      }
    }
  });
  socket.on("error", (error) => rejectAll(error));
  socket.on("close", () => rejectAll(new Error("Import Lens daemon connection closed")));

  const send = (message) => {
    const payload = Buffer.from(encode(message));
    const header = Buffer.allocUnsafe(4);
    header.writeUInt32BE(payload.length, 0);
    socket.write(Buffer.concat([header, payload]));
  };

  return {
    send,
    request: (message) =>
      new Promise((resolve, reject) => {
        const item = { resolve, reject, settled: false, timer: null };
        item.timer = setTimeout(
          () =>
            settle(item, () =>
              reject(new Error(`Import Lens daemon request timed out after ${requestTimeoutMs}ms`)),
            ),
          requestTimeoutMs,
        );
        pending.push(item);
        send(message);
      }),
  };
};

const deterministicPayload = (length) => {
  const alphabet = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
  let value = "";
  let state = 0x12345678;

  for (let index = 0; index < length; index += 1) {
    state = (Math.imul(state, 1664525) + 1013904223) >>> 0;
    value += alphabet[state % alphabet.length];
  }

  return value;
};

// Deterministic bytes that behave like a REAL binary under a compressor, which `deterministicBytes`
// does not: it takes `state & 0xff` from an LCG mod 2^32, and the low bits of such an LCG have
// period 2^k — so its low byte repeats every 256 bytes. Measured 2026-07-18: 6144 B and 10240 B of
// it both compress to 274 B (22:1 and 37:1). A wasm is entropy-dense and a woff2 is brotli-compressed
// internally; neither shrinks. Using the LCG helper for the binary fixture would have left its
// brotli axis measuring a compressible synthetic instead of a shipped binary.
//
// xorshift32's high byte is full-period: both sizes compress to length + 4 (ratio 1.000) at BOTH
// quality 4 and 11. That is what makes this the one benchmark with NO compressor-gap noise.
//
// Callers pass distinct seeds so two artifacts in one fixture are not prefixes of one another.
const incompressibleBytes = (length, seed) => {
  const value = Buffer.allocUnsafe(length);
  let state = seed;

  for (let index = 0; index < length; index += 1) {
    state ^= state << 13;
    state >>>= 0;
    state ^= state >>> 17;
    state ^= state << 5;
    state >>>= 0;
    value[index] = state >>> 24;
  }

  return value;
};

const deterministicBytes = (length) => {
  const value = Buffer.allocUnsafe(length);
  let state = 0x12345678;

  for (let index = 0; index < length; index += 1) {
    state = (Math.imul(state, 1664525) + 1013904223) >>> 0;
    value[index] = state & 0xff;
  }

  return value;
};

const brotliSize = (source) =>
  brotliCompressSync(source, {
    params: {
      [zlibConstants.BROTLI_PARAM_QUALITY]: 11,
    },
  }).length;

main().catch((error) => {
  process.stderr.write(`${error instanceof Error ? error.message : String(error)}\n`);
  process.exitCode = 1;
});
