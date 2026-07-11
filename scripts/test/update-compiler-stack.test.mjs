import assert from "node:assert/strict";
import { mkdtemp, readFile, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { compilerStackConfig } from "../compiler-stack.config.mjs";
import { fingerprintFromMetadata, formatFingerprint } from "../compiler-stack-fingerprint.mjs";
import { replaceKnownVersions, rolldownFamilyCrates } from "../compiler-stack-helpers.mjs";
import { runSafeUpdate } from "../deps-update-safe.mjs";
import { parseUpdateArgs, updateCompilerStack } from "../update-compiler-stack.mjs";

// The fixtures below stand in for a repo sitting on the CURRENT pins, because
// `replaceKnownVersions` looks for exactly those versions when it rewrites the
// SRS. Derive them from compiler-stack.config.mjs -- the single source of
// truth -- instead of typing them out, or every stack upgrade silently breaks
// this file.
const currentRolldown = compilerStackConfig.currentRolldownVersion;
const currentOxc = compilerStackConfig.currentOxcVersion;
const currentResolver = compilerStackConfig.currentResolverVersion;

// A synthetic upgrade target, always one minor ahead of whatever is pinned
// today, so it can never coincide with the current version and let "nothing
// changed" pass for a successful upgrade.
const nextMinor = (version) => {
  const [major, minor] = version.split(".").map(Number);
  return `${major}.${minor + 1}.0`;
};
const targetRolldown = nextMinor(currentRolldown);
const targetOxc = nextMinor(currentOxc);
const targetResolver = nextMinor(currentResolver);
const rolldownFamily = rolldownFamilyCrates();

// Versions are digits and dots; only the dots need escaping to embed one in a regex.
const escapeVersion = (version) => version.replaceAll(".", "\\.");

// Minimal `cargo metadata` shape shared by the probe resolution and the
// fingerprint recompute.
const probeMetadata = ({ rolldown, oxc, resolver }) => ({
  packages: [
    { id: "id:rolldown", name: "rolldown", version: rolldown, source: "registry+crates-io" },
    { id: "id:oxc_parser", name: "oxc_parser", version: oxc, source: "registry+crates-io" },
    {
      id: "id:oxc_resolver",
      name: "oxc_resolver",
      version: resolver,
      source: "registry+crates-io",
    },
  ],
  resolve: {
    nodes: [
      { id: "id:rolldown", dependencies: ["id:oxc_parser", "id:oxc_resolver"] },
      { id: "id:oxc_parser", dependencies: [] },
      { id: "id:oxc_resolver", dependencies: [] },
    ],
  },
});

const isCargoMetadata = (command, args) =>
  command === "cargo" && Array.isArray(args) && args[0] === "metadata";

// Records every exec; answers any `cargo metadata` with the given payload.
const cargoAwareExec = (calls, metadata) => async (command, args, options) => {
  calls.push(options === undefined ? [command, args] : [command, args, options]);
  if (isCargoMetadata(command, args)) {
    return { stdout: JSON.stringify(metadata) };
  }
  return { stdout: "" };
};

const probeLifecycle = () => {
  const state = { made: 0, removed: [] };
  return {
    state,
    mkdtemp: async (prefix) => {
      state.made += 1;
      return `${prefix}fixture`;
    },
    rm: async (target) => {
      state.removed.push(target);
    },
    probeWriteFile: async () => {},
  };
};

const tempRepo = async ({
  cargoToml = cargoTomlFixture(),
  manifest = manifestFixture(),
  srs = srsFixture(),
  fingerprint = formatFingerprint(
    fingerprintFromMetadata(
      probeMetadata({ rolldown: currentRolldown, oxc: currentOxc, resolver: currentResolver }),
    ),
  ),
} = {}) => {
  const root = await mkdtemp(path.join(os.tmpdir(), "importlens-compiler-update-"));
  await writeFile(path.join(root, "daemon-Cargo.toml"), cargoToml, "utf8");
  await writeFile(
    path.join(root, "package.json"),
    `${JSON.stringify(manifest, null, 2)}\n`,
    "utf8",
  );
  await writeFile(path.join(root, "ImportLens-SRS.md"), srs, "utf8");
  await writeFile(path.join(root, "compiler-stack.config.mjs"), configFixture(), "utf8");
  await writeFile(path.join(root, "compiler-stack.fingerprint.json"), fingerprint, "utf8");

  return {
    root,
    paths: {
      cargoToml: "daemon-Cargo.toml",
      manifest: "package.json",
      srs: "ImportLens-SRS.md",
      config: "compiler-stack.config.mjs",
      fingerprint: "compiler-stack.fingerprint.json",
    },
  };
};

test("replaceKnownVersions updates pinned tokens without touching substrings or overlapping versions", () => {
  const content = [
    `| \`rolldown\` | ${currentRolldown} | exact pin |`,
    `| \`oxc_parser\` | ${currentOxc} | exact pin |`,
    `currently resolved to ${currentResolver}.`,
    `unrelated build number 10${currentOxc}9 must survive`,
  ].join("\n");

  const lines = replaceKnownVersions(content, {
    rolldownVersion: targetRolldown,
    oxcVersion: targetOxc,
    resolverVersion: targetResolver,
  }).split("\n");

  assert.equal(lines[0], `| \`rolldown\` | ${targetRolldown} | exact pin |`);
  assert.equal(lines[1], `| \`oxc_parser\` | ${targetOxc} | exact pin |`);
  assert.equal(lines[2], `currently resolved to ${targetResolver}.`);
  // The old version embedded inside a longer number is not a pinned token.
  assert.equal(lines[3], `unrelated build number 10${currentOxc}9 must survive`);

  // A new oxc version that embeds the old resolver version must not then be
  // corrupted by the resolver replacement (a chained replaceAll would be).
  assert.equal(
    replaceKnownVersions(`pin ${currentOxc}`, {
      rolldownVersion: targetRolldown,
      oxcVersion: `${currentResolver}-oxc`,
      resolverVersion: targetResolver,
    }),
    `pin ${currentResolver}-oxc`,
  );
});

test("parseUpdateArgs supports explicit versions and dry-run", () => {
  assert.deepEqual(
    parseUpdateArgs([
      "--rolldown",
      targetRolldown,
      "--oxc",
      targetOxc,
      "--resolver",
      targetResolver,
      "--dry-run",
    ]),
    {
      dryRun: true,
      rolldownVersion: targetRolldown,
      oxcVersion: targetOxc,
      resolverVersion: targetResolver,
    },
  );
});

test("parseUpdateArgs ignores a bare -- separator", () => {
  // npm needs `--` to forward flags to the script; pnpm forwards the `--`
  // itself. Both invocations must parse identically.
  assert.deepEqual(parseUpdateArgs(["--", "--rolldown", targetRolldown, "--dry-run"]), {
    dryRun: true,
    rolldownVersion: targetRolldown,
    oxcVersion: undefined,
    resolverVersion: undefined,
  });
});

test("parseUpdateArgs still rejects an unknown option", () => {
  assert.throws(() => parseUpdateArgs(["--nope"]), /Unknown option: --nope/u);
});

test("dry-run resolves through the probe but writes no file and touches no lockfile", async () => {
  const repo = await tempRepo();
  const writes = [];
  const execs = [];
  const probe = probeLifecycle();

  const result = await updateCompilerStack({
    rootDir: repo.root,
    paths: repo.paths,
    dryRun: true,
    rolldownVersion: targetRolldown,
    oxcVersion: targetOxc,
    resolverVersion: targetResolver,
    fetchJson: availableVersions(),
    writeFile: async (...args) => writes.push(args),
    execFile: cargoAwareExec(
      execs,
      probeMetadata({ rolldown: targetRolldown, oxc: targetOxc, resolver: targetResolver }),
    ),
    ...probe,
  });

  assert.equal(result.rolldownVersion, targetRolldown);
  assert.equal(result.oxcVersion, targetOxc);
  assert.equal(result.resolverVersion, targetResolver);
  assert.deepEqual(
    result.changedFiles.sort(),
    [repo.paths.cargoToml, repo.paths.config, repo.paths.manifest, repo.paths.srs].sort(),
  );
  assert.deepEqual(writes, []);
  // Exactly one exec: the probe `cargo metadata`. No pnpm, no `cargo update`,
  // no fingerprint recompute.
  assert.equal(execs.length, 1);
  assert.ok(isCargoMetadata(execs[0][0], execs[0][1]));
  assert.deepEqual(probe.state.removed.length, 1);
  assert.match(
    await readFile(path.join(repo.root, repo.paths.cargoToml), "utf8"),
    new RegExp(`oxc_parser = "=${escapeVersion(currentOxc)}"`, "u"),
  );
});

test("updateCompilerStack updates manifests, SRS, config, lockfiles, and the fingerprint", async () => {
  const repo = await tempRepo();
  const execs = [];
  const probe = probeLifecycle();
  const metadata = probeMetadata({
    rolldown: targetRolldown,
    oxc: targetOxc,
    resolver: targetResolver,
  });

  const result = await updateCompilerStack({
    rootDir: repo.root,
    paths: repo.paths,
    rolldownVersion: targetRolldown,
    fetchJson: availableVersions(),
    platform: "linux",
    execFile: cargoAwareExec(execs, metadata),
    ...probe,
  });

  const cargoToml = await readFile(path.join(repo.root, repo.paths.cargoToml), "utf8");
  const manifest = JSON.parse(await readFile(path.join(repo.root, repo.paths.manifest), "utf8"));
  const srs = await readFile(path.join(repo.root, repo.paths.srs), "utf8");
  const config = await readFile(path.join(repo.root, repo.paths.config), "utf8");
  const fingerprint = await readFile(path.join(repo.root, repo.paths.fingerprint), "utf8");

  for (const crate of compilerStackConfig.oxcCrates) {
    assert.match(cargoToml, new RegExp(`^${crate} = "=${escapeVersion(targetOxc)}"$`, "mu"));
  }
  assert.match(cargoToml, new RegExp(`^oxc_resolver = "=${escapeVersion(targetResolver)}"$`, "mu"));
  for (const crate of rolldownFamily) {
    assert.match(cargoToml, new RegExp(`^${crate} = "=${escapeVersion(targetRolldown)}"$`, "mu"));
  }
  assert.equal(manifest.scripts["deps:update:compiler"], "node scripts/update-compiler-stack.mjs");
  assert.equal(manifest.scripts["deps:update:safe"], "node scripts/deps-update-safe.mjs");
  assert.match(srs, new RegExp(escapeVersion(targetRolldown), "u"));
  assert.match(srs, new RegExp(escapeVersion(targetOxc), "u"));
  assert.match(
    config,
    new RegExp(`currentRolldownVersion: "${escapeVersion(targetRolldown)}"`, "u"),
  );
  assert.match(config, new RegExp(`currentOxcVersion: "${escapeVersion(targetOxc)}"`, "u"));
  assert.equal(fingerprint, formatFingerprint(fingerprintFromMetadata(metadata)));
  assert.ok(result.changedFiles.includes(repo.paths.fingerprint));

  // Exec order: probe metadata, pnpm lockfile, precise pins (the rolldown
  // family, resolver, every oxc crate), fingerprint metadata.
  assert.ok(isCargoMetadata(execs[0][0], execs[0][1]));
  assert.deepEqual(execs[1], ["pnpm", ["install", "--lockfile-only"], { cwd: repo.root }]);
  assert.deepEqual(
    execs.slice(2, 2 + rolldownFamily.length),
    rolldownFamily.map((crate) => [
      "cargo",
      ["update", "-p", crate, "--precise", targetRolldown],
      { cwd: repo.root },
    ]),
  );
  assert.deepEqual(execs[2 + rolldownFamily.length], [
    "cargo",
    ["update", "-p", "oxc_resolver", "--precise", targetResolver],
    { cwd: repo.root },
  ]);
  const oxcOffset = 3 + rolldownFamily.length;
  const crateUpdates = execs.slice(oxcOffset, oxcOffset + compilerStackConfig.oxcCrates.length);
  assert.deepEqual(
    crateUpdates,
    compilerStackConfig.oxcCrates.map((crate) => [
      "cargo",
      ["update", "-p", crate, "--precise", targetOxc],
      { cwd: repo.root },
    ]),
  );
  const last = execs.at(-1);
  assert.ok(isCargoMetadata(last[0], last[1]));
  assert.ok(last[1].includes("--locked"));
});

test("updateCompilerStack launches pnpm through a shell on Windows for the lockfile update", async () => {
  const repo = await tempRepo();
  const execs = [];
  const probe = probeLifecycle();

  await updateCompilerStack({
    rootDir: repo.root,
    paths: repo.paths,
    rolldownVersion: targetRolldown,
    fetchJson: availableVersions(),
    platform: "win32",
    execFile: cargoAwareExec(
      execs,
      probeMetadata({ rolldown: targetRolldown, oxc: targetOxc, resolver: targetResolver }),
    ),
    ...probe,
  });

  // pnpm resolves to pnpm.CMD on Windows; it must go through the shell. cargo
  // is a real executable and stays on execFile without a shell.
  assert.deepEqual(execs[1], ["pnpm install --lockfile-only", { shell: true, cwd: repo.root }]);
  assert.deepEqual(execs[2], [
    "cargo",
    ["update", "-p", "rolldown", "--precise", targetRolldown],
    { cwd: repo.root },
  ]);
});

test("updateCompilerStack resolves the latest rolldown before probing", async () => {
  const repo = await tempRepo();
  const probe = probeLifecycle();

  const result = await updateCompilerStack({
    rootDir: repo.root,
    paths: repo.paths,
    dryRun: true,
    fetchJson: async (url) => {
      if (url.endsWith("/rolldown")) {
        return { crate: { max_stable_version: targetRolldown }, versions: [] };
      }
      return availableVersionPayload(url);
    },
    execFile: cargoAwareExec(
      [],
      probeMetadata({ rolldown: targetRolldown, oxc: currentOxc, resolver: currentResolver }),
    ),
    ...probe,
  });

  assert.equal(result.rolldownVersion, targetRolldown);
  assert.equal(result.oxcVersion, currentOxc);
  assert.equal(result.resolverVersion, currentResolver);
});

test("updateCompilerStack reports no changed files when the stack already matches", async () => {
  const manifest = manifestFixture();
  manifest.scripts = {
    "deps:update:compiler": "node scripts/update-compiler-stack.mjs",
    "deps:update:safe": "node scripts/deps-update-safe.mjs",
  };
  const repo = await tempRepo({ manifest });
  const probe = probeLifecycle();

  const result = await updateCompilerStack({
    rootDir: repo.root,
    paths: repo.paths,
    dryRun: true,
    rolldownVersion: currentRolldown,
    fetchJson: availableVersions(),
    execFile: cargoAwareExec(
      [],
      probeMetadata({ rolldown: currentRolldown, oxc: currentOxc, resolver: currentResolver }),
    ),
    ...probe,
  });

  assert.deepEqual(result.changedFiles, []);
});

test("updateCompilerStack rejects invalid or unavailable versions before edits", async () => {
  const repo = await tempRepo();
  const before = await readFile(path.join(repo.root, repo.paths.cargoToml), "utf8");
  const probe = probeLifecycle();

  await assert.rejects(
    updateCompilerStack({
      rootDir: repo.root,
      paths: repo.paths,
      rolldownVersion: targetRolldown,
      oxcVersion: "latest",
      fetchJson: availableVersions(),
      execFile: cargoAwareExec([], probeMetadata({})),
      ...probe,
    }),
    /Invalid OXC version/u,
  );

  await assert.rejects(
    updateCompilerStack({
      rootDir: repo.root,
      paths: repo.paths,
      rolldownVersion: targetRolldown,
      oxcVersion: "0.999.0",
      fetchJson: availableVersions(),
      execFile: cargoAwareExec(
        [],
        probeMetadata({ rolldown: targetRolldown, oxc: "0.999.0", resolver: currentResolver }),
      ),
      ...probe,
    }),
    /Unavailable OXC crate/u,
  );

  assert.equal(await readFile(path.join(repo.root, repo.paths.cargoToml), "utf8"), before);
});

test("updateCompilerStack rejects an unsatisfiable probe before any edit", async () => {
  const repo = await tempRepo();
  const before = await readFile(path.join(repo.root, repo.paths.cargoToml), "utf8");
  const writes = [];
  const probe = probeLifecycle();

  await assert.rejects(
    updateCompilerStack({
      rootDir: repo.root,
      paths: repo.paths,
      rolldownVersion: targetRolldown,
      oxcVersion: targetOxc,
      fetchJson: availableVersions(),
      writeFile: async (...args) => writes.push(args),
      execFile: async (command, args) => {
        if (isCargoMetadata(command, args)) {
          const error = new Error("cargo metadata failed");
          error.stderr = "error: failed to select a version for oxc_parser";
          throw error;
        }
        return { stdout: "" };
      },
      ...probe,
    }),
    /Unsatisfiable compiler stack:[\s\S]*failed to select a version/u,
  );

  assert.deepEqual(writes, []);
  // The temp probe directory is removed even on failure.
  assert.equal(probe.state.removed.length, 1);
  assert.equal(await readFile(path.join(repo.root, repo.paths.cargoToml), "utf8"), before);
});

test("updateCompilerStack rejects when Cargo resolves a different version than requested", async () => {
  const repo = await tempRepo();
  const probe = probeLifecycle();

  await assert.rejects(
    updateCompilerStack({
      rootDir: repo.root,
      paths: repo.paths,
      rolldownVersion: targetRolldown,
      oxcVersion: targetOxc,
      fetchJson: availableVersions(),
      execFile: cargoAwareExec(
        [],
        probeMetadata({ rolldown: targetRolldown, oxc: currentOxc, resolver: currentResolver }),
      ),
      ...probe,
    }),
    /Cargo resolved OXC/u,
  );
});

test("updateCompilerStack rejects a probe graph that resolves a coordinated crate twice", async () => {
  // Cargo duplicates semver-incompatible copies instead of failing, so an
  // explicit override fighting rolldown's own requirement "resolves". The
  // updater must reject the split stack before any tracked edit.
  const repo = await tempRepo();
  const writes = [];
  const probe = probeLifecycle();
  const metadata = probeMetadata({
    rolldown: targetRolldown,
    oxc: currentOxc,
    resolver: currentResolver,
  });
  metadata.packages.push({
    id: "id:oxc_parser-dup",
    name: "oxc_parser",
    version: targetOxc,
    source: "registry+crates-io",
  });

  await assert.rejects(
    updateCompilerStack({
      rootDir: repo.root,
      paths: repo.paths,
      rolldownVersion: targetRolldown,
      oxcVersion: currentOxc,
      fetchJson: availableVersions(),
      writeFile: async (...args) => writes.push(args),
      execFile: cargoAwareExec([], metadata),
      ...probe,
    }),
    /Unsatisfiable compiler stack: oxc_parser resolves to multiple versions/u,
  );

  assert.deepEqual(writes, []);
  assert.equal(probe.state.removed.length, 1);
});

test("updateCompilerStack rejects a manifest without the coordinated shape before edits", async () => {
  const nonCoordinated = await tempRepo({
    cargoToml: cargoTomlFixture().replace(`oxc_parser = "=${currentOxc}"`, 'oxc_parser = "=0.0.1"'),
  });
  const withMangler = await tempRepo({
    cargoToml: `${cargoTomlFixture()}oxc_mangler = "=${currentOxc}"\n`,
  });
  const withoutRolldown = await tempRepo({
    cargoToml: cargoTomlFixture().replace(`rolldown = "=${currentRolldown}"\n`, ""),
  });

  for (const [repo, message] of [
    [nonCoordinated, /Current OXC crate versions are not coordinated/u],
    [withMangler, /oxc_mangler must not be present/u],
    [withoutRolldown, /Missing exact pin \(=\) for rolldown/u],
  ]) {
    await assert.rejects(
      updateCompilerStack({
        rootDir: repo.root,
        paths: repo.paths,
        rolldownVersion: targetRolldown,
        fetchJson: availableVersions(),
        execFile: cargoAwareExec(
          [],
          probeMetadata({ rolldown: targetRolldown, oxc: targetOxc, resolver: targetResolver }),
        ),
        ...probeLifecycle(),
      }),
      message,
    );
  }
});

test("the probe manifest pins every rolldown-family crate at the requested version", async () => {
  // The probe is the gate that fails BEFORE any tracked edit when a rolldown
  // release ships without its sibling crates at the same version; that only
  // holds if the probe manifest actually constrains the siblings.
  const repo = await tempRepo();
  const probeWrites = [];
  const probe = probeLifecycle();

  await updateCompilerStack({
    rootDir: repo.root,
    paths: repo.paths,
    rolldownVersion: targetRolldown,
    dryRun: true,
    fetchJson: availableVersions(),
    platform: "linux",
    execFile: cargoAwareExec(
      [],
      probeMetadata({ rolldown: targetRolldown, oxc: targetOxc, resolver: targetResolver }),
    ),
    ...probe,
    probeWriteFile: async (file, content) => probeWrites.push([file, content]),
  });

  const manifest = probeWrites
    .map(([, content]) => content)
    .find((content) => typeof content === "string" && content.includes("[dependencies]"));
  assert.ok(manifest, "the probe should write a Cargo.toml manifest");
  for (const crate of rolldownFamily) {
    assert.match(
      manifest,
      new RegExp(`^${crate} = "=${escapeVersion(targetRolldown)}"$`, "mu"),
      `probe manifest must pin ${crate} at the requested rolldown version`,
    );
  }
});

test("updateCompilerStack rejects a rolldown release whose support crates are unavailable", async () => {
  const repo = await tempRepo();
  const writes = [];

  await assert.rejects(
    updateCompilerStack({
      rootDir: repo.root,
      paths: repo.paths,
      rolldownVersion: targetRolldown,
      fetchJson: async (url) => {
        if (url.includes("/rolldown_common/")) {
          throw new Error("404 Not Found");
        }
        return availableVersionPayload(url);
      },
      platform: "linux",
      execFile: cargoAwareExec(
        [],
        probeMetadata({ rolldown: targetRolldown, oxc: targetOxc, resolver: targetResolver }),
      ),
      writeFile: async (...args) => writes.push(args),
      ...probeLifecycle(),
    }),
    /Unavailable rolldown_common version/u,
  );

  assert.deepEqual(writes, []);
});

test("runSafeUpdate restores the stack and validates the fingerprint", async () => {
  const metadata = probeMetadata({
    rolldown: currentRolldown,
    oxc: currentOxc,
    resolver: currentResolver,
  });
  const committed = formatFingerprint(fingerprintFromMetadata(metadata));
  const execs = [];

  await runSafeUpdate({
    rootDir: "/repo",
    platform: "linux",
    execFile: cargoAwareExec(execs, metadata),
    readFile: async () => committed,
  });

  assert.deepEqual(execs[0], ["pnpm", ["update"], { cwd: "/repo" }]);
  assert.deepEqual(execs[1], ["cargo", ["update"], { cwd: "/repo" }]);
  assert.deepEqual(
    execs.slice(2, 2 + rolldownFamily.length),
    rolldownFamily.map((crate) => [
      "cargo",
      ["update", "-p", crate, "--precise", currentRolldown],
      { cwd: "/repo" },
    ]),
  );
  assert.deepEqual(execs[2 + rolldownFamily.length], [
    "cargo",
    ["update", "-p", "oxc_resolver", "--precise", currentResolver],
    { cwd: "/repo" },
  ]);
  const restoreOffset = 3 + rolldownFamily.length;
  const crateRestores = execs.slice(
    restoreOffset,
    restoreOffset + compilerStackConfig.oxcCrates.length,
  );
  assert.deepEqual(
    crateRestores,
    compilerStackConfig.oxcCrates.map((crate) => [
      "cargo",
      ["update", "-p", crate, "--precise", currentOxc],
      { cwd: "/repo" },
    ]),
  );
  const last = execs.at(-1);
  assert.ok(isCargoMetadata(last[0], last[1]));
});

test("runSafeUpdate fails when the restored graph no longer matches the fingerprint", async () => {
  const metadata = probeMetadata({
    rolldown: currentRolldown,
    oxc: currentOxc,
    resolver: currentResolver,
  });
  const moved = probeMetadata({
    rolldown: currentRolldown,
    oxc: targetOxc,
    resolver: currentResolver,
  });

  await assert.rejects(
    runSafeUpdate({
      rootDir: "/repo",
      platform: "linux",
      execFile: cargoAwareExec([], moved),
      readFile: async () => formatFingerprint(fingerprintFromMetadata(metadata)),
    }),
    /could not restore the recorded compiler stack/u,
  );
});

const availableVersions = () => async (url) => availableVersionPayload(url);

const availableVersionPayload = (url) => {
  for (const crate of rolldownFamily) {
    for (const version of [currentRolldown, targetRolldown]) {
      if (url.endsWith(`/${crate}/${version}`)) {
        return { version: { num: version } };
      }
    }
  }
  if (url.endsWith("/rolldown")) {
    return { crate: { max_stable_version: targetRolldown }, versions: [] };
  }
  for (const version of [currentResolver, targetResolver]) {
    if (url.endsWith(`/oxc_resolver/${version}`)) {
      return { version: { num: version } };
    }
  }
  if (url.endsWith("/oxc_resolver")) {
    return { crate: { max_stable_version: targetResolver }, versions: [] };
  }
  for (const version of [currentOxc, targetOxc]) {
    if (compilerStackConfig.oxcCrates.some((crate) => url.endsWith(`/${crate}/${version}`))) {
      return { version: { num: version } };
    }
  }
  throw new Error(`unexpected fetch: ${url}`);
};

const cargoTomlFixture = () => `[dependencies]
brotli = "^8"
${compilerStackConfig.oxcCrates.map((crate) => `${crate} = "=${currentOxc}"`).join("\n")}
oxc_resolver = "=${currentResolver}"
${rolldownFamily.map((crate) => `${crate} = "=${currentRolldown}"`).join("\n")}
zstd = "^0.13"
`;

const manifestFixture = () => ({
  // Intentionally missing deps:update:compiler so updateManifest normalizes
  // it, which is what the "manifest is a changed file" assertions exercise.
  scripts: {
    "deps:update:safe": "pnpm update && cargo update",
  },
  dependencies: {
    "@msgpack/msgpack": "3.1.3",
  },
});

const srsFixture = () => `| \`rolldown\`        | ${currentRolldown} |
| \`oxc_parser\`      | ${currentOxc} |
| \`oxc_resolver\`    | ${currentResolver} |
currently resolved to ${currentOxc}
Currently resolved to ${currentResolver}
baseline v${currentOxc}
`;

const configFixture = () => `export const compilerStackConfig = {
  currentRolldownVersion: "${currentRolldown}",
  currentOxcVersion: "${currentOxc}",
  currentResolverVersion: "${currentResolver}",
};
`;
