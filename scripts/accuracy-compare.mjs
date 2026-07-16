#!/usr/bin/env node

// Fixture coverage, stated plainly so nobody mistakes a green run for more than it is:
//
//   synthetic (offline, deterministic)
//     - flat / branchy: tree-shaking behavior, incl. the only assertion that an
//       unreachable module is EXCLUDED from the breakdown.
//     - typescript package: the `graph.rs` TypeScript transform path, the only place
//       the daemon transforms real TS. A lowered `enum` and `namespace` both codegen
//       as IIFEs, so this doubles as coverage of the minifier's unused-IIFE analysis.
//   real packages (downloaded on demand, lockfile-pinned)
//     - css-tree: deep ESM graph with transitive dependencies.
//     - date-fns: deep zero-dependency ESM graph.
//     - lodash:   the CommonJS path -- `SourceType::cjs()` and the `;(() => {…})();`
//                 wrapper that `pipeline/cjs.rs` builds.
//     - refractor: a sideEffects glob anchored at the package root.
//     - @uiw/react-md-editor: the only real package whose published ESM entry actually
//                 does `import "./index.css"`, so it is the one benchmark that compares
//                 ASSET COUNTING (B2) against the oracle -- both sides must fold in the
//                 same stylesheet, or a missed `@import` / double count shows up here.
//
// NOT covered: the `.js`-containing-JSX retry path (`graph.rs`), which is a
// parse-failure fallback; and the mangler's exported-destructuring handling, which
// `pipeline/bundle.rs` puts out of reach by stripping `export ` before minification.
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
// Maximum accepted brotli delta against the esbuild oracle, as a fraction.
//
// Derivation: the worst delta observed across every benchmark on the current
// engine (2026-07-11 re-baseline) is 13.0%; the rest sit at 2.6-12%. 25% is
// that worst case doubled and rounded down to a round number, so a legitimate
// compiler-stack bump has ~2x the observed spread of headroom before it turns
// CI red for a non-bug, while a real regression (a dangling binding dragging a
// dead module into the bundle) still moves the number far past it. The former
// 75% default could not fail on anything short of a catastrophe.
//
// Re-derive this the next time the observed worst case moves: keep it at
// roughly twice the worst accepted delta, and never raise it to make a red run
// green without first proving the delta is a codegen difference, not a bug.
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
  // gates asset counting (B2) against the oracle: both sides must fold in the same stylesheet, so a
  // missed `@import`, a double count, or an asymmetric inline shows up here as a delta rather than
  // as a wrong number in the product.
  {
    package: "@uiw/react-md-editor",
    named: "headingExecute",
    label: "@uiw/react-md-editor (ESM entry imports CSS: asset counting vs the oracle)",
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
      ...(realFixtureState.installed
        ? await writeRealFixtureEntries(workspace, realFixtureState.versions)
        : []),
    ];

    daemon = await startDaemon(workspace);

    for (const [index, benchmark] of benchmarks.entries()) {
      const importLens = await importLensNamedSize(daemon, workspace, benchmark, index + 1);
      const esbuildSize = await esbuildNamedSize(workspace, benchmark.activeDocumentPath);
      const delta = Math.abs(importLens.brotliBytes - esbuildSize.brotliBytes);
      const relativeDelta = delta / Math.max(esbuildSize.brotliBytes, 1);

      process.stdout.write(
        [
          `${benchmark.label}:`,
          `  Import Lens named import: ${importLens.brotliBytes} B br (${importLens.minifiedBytes} B minified)`,
          `  esbuild named import: ${esbuildSize.brotliBytes} B br (${esbuildSize.minifiedBytes} B minified)`,
          `  relative delta: ${(relativeDelta * 100).toFixed(1)}%`,
        ].join("\n"),
      );
      process.stdout.write("\n");

      if (relativeDelta > tolerance) {
        throw new Error(
          `${benchmark.label} accuracy delta ${(relativeDelta * 100).toFixed(1)}% exceeds ${(tolerance * 100).toFixed(1)}% tolerance`,
        );
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

  return versions;
};

const writeRealFixtureEntries = async (workspace, versions) => {
  const sourceRoot = path.join(workspace, "src");
  const benchmarks = [];

  for (const fixture of realFixtures) {
    const activeDocumentPath = path.join(sourceRoot, `real-${fixture.package}-entry.js`);
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
  return { flatActiveDocumentPath, branchyActiveDocumentPath, typedActiveDocumentPath };
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
  };
};

const esbuildNamedSize = async (workspace, activeDocumentPath) => {
  const result = await esbuild.build({
    absWorkingDir: workspace,
    entryPoints: [activeDocumentPath],
    bundle: true,
    minify: true,
    write: false,
    format: "esm",
    platform: "browser",
    treeShaking: true,
    logLevel: "silent",
  });

  // When the bundled graph imports CSS, esbuild gathers it into a SIBLING `.css` output beside the
  // JS chunk, so `outputFiles` holds more than one entry and the JS is not guaranteed to be at
  // index 0. The daemon counts those stylesheet bytes now (B2), so the oracle must too, or the two
  // would be measuring different things and the comparison would be meaningless.
  //
  // Classify by extension and compress each artifact ON ITS OWN before summing — never concatenate
  // first — because that is exactly what the daemon does (ADR-0005: they are separate files that
  // ship separately). `reduce` over an empty CSS list is zero, so a pure-JS benchmark is unchanged.
  const javascript = result.outputFiles.filter((file) => file.path.endsWith(".js"));
  const stylesheets = result.outputFiles.filter((file) => file.path.endsWith(".css"));

  if (javascript.length === 0) {
    throw new Error("esbuild did not produce a JavaScript output file");
  }

  const bytesOf = (files) => files.reduce((bytes, file) => bytes + file.contents.length, 0);
  const brotliBytesOf = (files) =>
    files.reduce((bytes, file) => bytes + brotliSize(file.contents), 0);

  return {
    brotliBytes: brotliBytesOf(javascript) + brotliBytesOf(stylesheets),
    minifiedBytes: bytesOf(javascript) + bytesOf(stylesheets),
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
