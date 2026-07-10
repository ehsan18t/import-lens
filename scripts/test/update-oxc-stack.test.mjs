import assert from "node:assert/strict";
import { mkdtemp, readFile, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { oxcStackConfig } from "../oxc-stack.config.mjs";
import { replaceKnownVersions } from "../oxc-stack-helpers.mjs";
import { parseUpdateArgs, updateOxcStack } from "../update-oxc-stack.mjs";

// The fixtures below stand in for a repo sitting on the CURRENT pins, because
// `replaceKnownVersions` looks for exactly those versions when it rewrites the SRS.
// Derive them from oxc-stack.config.mjs -- the single source of truth -- instead of
// typing them out, or every OXC upgrade silently breaks this file.
const currentOxc = oxcStackConfig.currentOxcVersion;
const currentResolver = oxcStackConfig.currentResolverVersion;

// A synthetic upgrade target, always one minor ahead of whatever is pinned today, so
// it can never coincide with the current version and let "nothing changed" pass for a
// successful upgrade.
const nextMinor = (version) => {
  const [major, minor] = version.split(".").map(Number);
  return `${major}.${minor + 1}.0`;
};
const targetOxc = nextMinor(currentOxc);
const targetResolver = nextMinor(currentResolver);

// Versions are digits and dots; only the dots need escaping to embed one in a regex.
const escapeVersion = (version) => version.replaceAll(".", "\\.");

const tempRepo = async ({
  cargoToml = cargoTomlFixture(),
  manifest = manifestFixture(),
  srs = srsFixture(),
} = {}) => {
  const root = await mkdtemp(path.join(os.tmpdir(), "importlens-oxc-update-"));
  await writeFile(path.join(root, "daemon-Cargo.toml"), cargoToml, "utf8");
  await writeFile(
    path.join(root, "package.json"),
    `${JSON.stringify(manifest, null, 2)}\n`,
    "utf8",
  );
  await writeFile(path.join(root, "ImportLens-SRS.md"), srs, "utf8");
  await writeFile(path.join(root, "oxc-stack.config.mjs"), configFixture(), "utf8");

  return {
    root,
    paths: {
      cargoToml: "daemon-Cargo.toml",
      manifest: "package.json",
      srs: "ImportLens-SRS.md",
      config: "oxc-stack.config.mjs",
    },
  };
};

test("replaceKnownVersions updates pinned tokens without touching substrings or overlapping versions", () => {
  const oldOxc = oxcStackConfig.currentOxcVersion;
  const oldResolver = oxcStackConfig.currentResolverVersion;

  const content = [
    `| \`oxc_parser\` | ${oldOxc} | patch pin |`,
    `currently resolved to ${oldResolver}.`,
    `unrelated build number 10${oldOxc}9 must survive`,
  ].join("\n");

  const lines = replaceKnownVersions(content, "0.139.0", "11.23.0").split("\n");

  assert.equal(lines[0], "| `oxc_parser` | 0.139.0 | patch pin |");
  assert.equal(lines[1], "currently resolved to 11.23.0.");
  // The old version embedded inside a longer number is not a pinned token.
  assert.equal(lines[2], `unrelated build number 10${oldOxc}9 must survive`);

  // A new oxc version that embeds the old resolver version must not then be
  // corrupted by the resolver replacement (a chained replaceAll would be).
  assert.equal(
    replaceKnownVersions(`pin ${oldOxc}`, `${oldResolver}-oxc`, "11.23.0"),
    `pin ${oldResolver}-oxc`,
  );
});

test("parseUpdateArgs supports explicit versions and dry-run", () => {
  assert.deepEqual(parseUpdateArgs(["--oxc", "0.139.0", "--resolver", "11.22.0", "--dry-run"]), {
    dryRun: true,
    oxcVersion: "0.139.0",
    resolverVersion: "11.22.0",
  });
});

test("parseUpdateArgs ignores a bare -- separator", () => {
  // npm needs `--` to forward flags to the script; pnpm forwards the `--` itself.
  // Both invocations must parse identically, or the documented command fails.
  assert.deepEqual(parseUpdateArgs(["--", "--oxc", "0.139.0", "--dry-run"]), {
    dryRun: true,
    oxcVersion: "0.139.0",
    resolverVersion: undefined,
  });
});

test("parseUpdateArgs still rejects an unknown option", () => {
  assert.throws(() => parseUpdateArgs(["--nope"]), /Unknown option: --nope/u);
});

test("updateOxcStack dry-run reports planned edits without writing files or lockfiles", async () => {
  const repo = await tempRepo();
  const writes = [];
  const execs = [];

  const result = await updateOxcStack({
    rootDir: repo.root,
    paths: repo.paths,
    dryRun: true,
    oxcVersion: targetOxc,
    resolverVersion: currentResolver,
    fetchJson: availableVersions(),
    writeFile: async (...args) => writes.push(args),
    execFile: async (...args) => execs.push(args),
  });

  assert.equal(result.oxcVersion, targetOxc);
  assert.equal(result.resolverVersion, currentResolver);
  assert.deepEqual(
    result.changedFiles.sort(),
    [repo.paths.cargoToml, repo.paths.config, repo.paths.manifest, repo.paths.srs].sort(),
  );
  assert.deepEqual(writes, []);
  assert.deepEqual(execs, []);
  assert.match(
    await readFile(path.join(repo.root, repo.paths.cargoToml), "utf8"),
    new RegExp(`oxc_parser = "~${escapeVersion(currentOxc)}"`, "u"),
  );
});

test("updateOxcStack updates manifests, SRS, config, and lockfiles", async () => {
  const repo = await tempRepo();
  const execs = [];

  await updateOxcStack({
    rootDir: repo.root,
    paths: repo.paths,
    oxcVersion: targetOxc,
    resolverVersion: currentResolver,
    fetchJson: availableVersions(),
    platform: "linux",
    execFile: async (command, args) => execs.push([command, args]),
  });

  const cargoToml = await readFile(path.join(repo.root, repo.paths.cargoToml), "utf8");
  const manifest = JSON.parse(await readFile(path.join(repo.root, repo.paths.manifest), "utf8"));
  const srs = await readFile(path.join(repo.root, repo.paths.srs), "utf8");
  const config = await readFile(path.join(repo.root, repo.paths.config), "utf8");

  for (const crate of oxcStackConfig.oxcCrates) {
    assert.match(cargoToml, new RegExp(`^${crate} = "~${escapeVersion(targetOxc)}"$`, "mu"));
  }

  assert.match(
    cargoToml,
    new RegExp(`^oxc_resolver = "~${escapeVersion(currentResolver)}"$`, "mu"),
  );
  assert.equal(manifest.dependencies["oxc-parser"], undefined);
  assert.match(srs, new RegExp(escapeVersion(targetOxc), "u"));
  assert.match(srs, new RegExp(escapeVersion(currentResolver), "u"));
  assert.match(config, new RegExp(`currentOxcVersion: "${escapeVersion(targetOxc)}"`, "u"));
  assert.match(
    config,
    new RegExp(`currentResolverVersion: "${escapeVersion(currentResolver)}"`, "u"),
  );
  assert.deepEqual(execs, [
    ["pnpm", ["install", "--lockfile-only"]],
    ["cargo", ["update", "-p", "oxc_resolver", "--precise", currentResolver]],
    ...oxcStackConfig.oxcCrates.map((crate) => [
      "cargo",
      ["update", "-p", crate, "--precise", targetOxc],
    ]),
  ]);
});

test("updateOxcStack launches pnpm through a shell on Windows for the lockfile update", async () => {
  const repo = await tempRepo();
  const execs = [];

  await updateOxcStack({
    rootDir: repo.root,
    paths: repo.paths,
    oxcVersion: targetOxc,
    resolverVersion: currentResolver,
    fetchJson: availableVersions(),
    platform: "win32",
    execFile: async (...args) => execs.push(args),
  });

  // pnpm resolves to pnpm.CMD on Windows; it must go through the shell.
  assert.deepEqual(execs[0], ["pnpm install --lockfile-only", { shell: true }]);
  // cargo is a real executable and stays on execFile without a shell.
  assert.deepEqual(execs[1], [
    "cargo",
    ["update", "-p", "oxc_resolver", "--precise", currentResolver],
  ]);
});

test("updateOxcStack resolves latest versions before editing", async () => {
  const repo = await tempRepo();

  const result = await updateOxcStack({
    rootDir: repo.root,
    paths: repo.paths,
    dryRun: true,
    fetchJson: async (url) => {
      if (url.endsWith("/oxc_parser")) {
        return { crate: { max_stable_version: currentOxc }, versions: [] };
      }
      if (url.endsWith("/oxc_resolver")) {
        return { crate: { max_stable_version: targetResolver }, versions: [] };
      }
      return availableVersionPayload(url);
    },
  });

  assert.equal(result.oxcVersion, currentOxc);
  assert.equal(result.resolverVersion, targetResolver);
});

test("updateOxcStack reports no changed files when target versions and scripts already match", async () => {
  const manifest = manifestFixture();
  manifest.scripts = {
    "deps:update:oxc": "node scripts/update-oxc-stack.mjs",
    "deps:update:safe": "pnpm update && cargo update",
  };
  const repo = await tempRepo({ manifest });

  const result = await updateOxcStack({
    rootDir: repo.root,
    paths: repo.paths,
    dryRun: true,
    oxcVersion: currentOxc,
    resolverVersion: currentResolver,
    fetchJson: availableVersions(),
  });

  assert.deepEqual(result.changedFiles, []);
});

test("updateOxcStack rejects invalid or unavailable versions before edits", async () => {
  const repo = await tempRepo();
  const before = await readFile(path.join(repo.root, repo.paths.cargoToml), "utf8");

  await assert.rejects(
    updateOxcStack({
      rootDir: repo.root,
      paths: repo.paths,
      oxcVersion: "latest",
      resolverVersion: currentResolver,
      fetchJson: availableVersions(),
    }),
    /Invalid OXC version/,
  );

  await assert.rejects(
    updateOxcStack({
      rootDir: repo.root,
      paths: repo.paths,
      oxcVersion: "0.999.0",
      resolverVersion: currentResolver,
      fetchJson: availableVersions(),
    }),
    /Unavailable OXC crate/,
  );

  assert.equal(await readFile(path.join(repo.root, repo.paths.cargoToml), "utf8"), before);
});

test("updateOxcStack rejects non-coordinated current OXC crates and oxc_mangler before edits", async () => {
  const nonCoordinated = await tempRepo({
    cargoToml: cargoTomlFixture().replace(`oxc_ast = "~${currentOxc}"`, 'oxc_ast = "~0.0.1"'),
  });
  const withMangler = await tempRepo({
    cargoToml: `${cargoTomlFixture()}oxc_mangler = "~${currentOxc}"\n`,
  });

  await assert.rejects(
    updateOxcStack({
      rootDir: nonCoordinated.root,
      paths: nonCoordinated.paths,
      oxcVersion: targetOxc,
      resolverVersion: currentResolver,
      fetchJson: availableVersions(),
    }),
    /Current OXC crate versions are not coordinated/,
  );

  await assert.rejects(
    updateOxcStack({
      rootDir: withMangler.root,
      paths: withMangler.paths,
      oxcVersion: targetOxc,
      resolverVersion: currentResolver,
      fetchJson: availableVersions(),
    }),
    /oxc_mangler must not be present/,
  );
});

const availableVersions = () => async (url) => availableVersionPayload(url);

const availableVersionPayload = (url) => {
  if (url.endsWith("/oxc_parser")) {
    return {
      crate: { max_stable_version: targetOxc },
      versions: [{ num: targetOxc }, { num: currentOxc }],
    };
  }
  if (url.endsWith("/oxc_resolver")) {
    return {
      crate: { max_stable_version: currentResolver },
      versions: [{ num: currentResolver }, { num: targetResolver }],
    };
  }
  for (const version of [currentResolver, targetResolver]) {
    if (url.endsWith(`/oxc_resolver/${version}`)) {
      return { version: { num: version } };
    }
  }
  for (const version of [currentOxc, targetOxc]) {
    if (oxcStackConfig.oxcCrates.some((crate) => url.endsWith(`/${crate}/${version}`))) {
      return { version: { num: version } };
    }
  }
  throw new Error(`unexpected fetch: ${url}`);
};

const cargoTomlFixture = () => `[dependencies]
brotli = "^8"
${oxcStackConfig.oxcCrates.map((crate) => `${crate} = "~${currentOxc}"`).join("\n")}
oxc_resolver = "~${currentResolver}"
zstd = "^0.13"
`;

const manifestFixture = () => ({
  // Intentionally missing deps:update:oxc so updateManifest normalizes it,
  // which is what the "manifest is a changed file" assertions exercise.
  scripts: {
    "deps:update:safe": "pnpm update && cargo update",
  },
  dependencies: {
    "@msgpack/msgpack": "3.1.3",
  },
});

const srsFixture = () => `| \`oxc_parser\`      | ${currentOxc} |
| \`oxc_resolver\`    | ${currentResolver} |
currently resolved to ${currentOxc}
Currently resolved to ${currentResolver}
baseline v${currentOxc}
`;

const configFixture = () => `export const oxcStackConfig = {
  currentOxcVersion: "${currentOxc}",
  currentResolverVersion: "${currentResolver}",
};
`;
