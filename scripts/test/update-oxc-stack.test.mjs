import assert from "node:assert/strict";
import { mkdtemp, readFile, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { oxcStackConfig } from "../oxc-stack.config.mjs";
import { replaceKnownVersions } from "../oxc-stack-helpers.mjs";
import { parseUpdateArgs, updateOxcStack } from "../update-oxc-stack.mjs";

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
  await writeFile(path.join(root, "dependency-policy.test.mjs"), dependencyPolicyFixture(), "utf8");
  await writeFile(path.join(root, "package-vsix-manifest.test.mjs"), packageVsixFixture(), "utf8");
  await writeFile(path.join(root, "ImportLens-SRS.md"), srs, "utf8");
  await writeFile(path.join(root, "oxc-stack.config.mjs"), configFixture(), "utf8");

  return {
    root,
    paths: {
      cargoToml: "daemon-Cargo.toml",
      manifest: "package.json",
      dependencyPolicyTest: "dependency-policy.test.mjs",
      packageVsixManifestTest: "package-vsix-manifest.test.mjs",
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

test("updateOxcStack dry-run reports planned edits without writing files or lockfiles", async () => {
  const repo = await tempRepo();
  const writes = [];
  const execs = [];

  const result = await updateOxcStack({
    rootDir: repo.root,
    paths: repo.paths,
    dryRun: true,
    oxcVersion: "0.139.0",
    resolverVersion: "11.22.0",
    fetchJson: availableVersions(),
    writeFile: async (...args) => writes.push(args),
    execFile: async (...args) => execs.push(args),
  });

  assert.equal(result.oxcVersion, "0.139.0");
  assert.equal(result.resolverVersion, "11.22.0");
  assert.deepEqual(
    result.changedFiles.sort(),
    [repo.paths.cargoToml, repo.paths.config, repo.paths.manifest, repo.paths.srs].sort(),
  );
  assert.deepEqual(writes, []);
  assert.deepEqual(execs, []);
  assert.match(
    await readFile(path.join(repo.root, repo.paths.cargoToml), "utf8"),
    /oxc_parser = "~0\.138\.0"/,
  );
});

test("updateOxcStack updates manifests, SRS, config, and lockfiles", async () => {
  const repo = await tempRepo();
  const execs = [];

  await updateOxcStack({
    rootDir: repo.root,
    paths: repo.paths,
    oxcVersion: "0.139.0",
    resolverVersion: "11.22.0",
    fetchJson: availableVersions(),
    platform: "linux",
    execFile: async (command, args) => execs.push([command, args]),
  });

  const cargoToml = await readFile(path.join(repo.root, repo.paths.cargoToml), "utf8");
  const manifest = JSON.parse(await readFile(path.join(repo.root, repo.paths.manifest), "utf8"));
  const packageVsix = await readFile(
    path.join(repo.root, repo.paths.packageVsixManifestTest),
    "utf8",
  );
  const srs = await readFile(path.join(repo.root, repo.paths.srs), "utf8");
  const config = await readFile(path.join(repo.root, repo.paths.config), "utf8");

  for (const crate of oxcStackConfig.oxcCrates) {
    assert.match(cargoToml, new RegExp(`^${crate} = "~0\\.139\\.0"$`, "mu"));
  }

  assert.match(cargoToml, /^oxc_resolver = "~11\.22\.0"$/mu);
  assert.equal(manifest.dependencies["oxc-parser"], undefined);
  assert.doesNotMatch(packageVsix, /oxc-parser/);
  assert.match(srs, /0\.139\.0/);
  assert.match(srs, /11\.22\.0/);
  assert.match(config, /currentOxcVersion: "0\.139\.0"/);
  assert.match(config, /currentResolverVersion: "11\.22\.0"/);
  assert.deepEqual(execs, [
    ["pnpm", ["install", "--lockfile-only"]],
    ["cargo", ["update", "-p", "oxc_resolver", "--precise", "11.22.0"]],
    ...oxcStackConfig.oxcCrates.map((crate) => [
      "cargo",
      ["update", "-p", crate, "--precise", "0.139.0"],
    ]),
  ]);
});

test("updateOxcStack launches pnpm through a shell on Windows for the lockfile update", async () => {
  const repo = await tempRepo();
  const execs = [];

  await updateOxcStack({
    rootDir: repo.root,
    paths: repo.paths,
    oxcVersion: "0.139.0",
    resolverVersion: "11.22.0",
    fetchJson: availableVersions(),
    platform: "win32",
    execFile: async (...args) => execs.push(args),
  });

  // pnpm resolves to pnpm.CMD on Windows; it must go through the shell.
  assert.deepEqual(execs[0], ["pnpm install --lockfile-only", { shell: true }]);
  // cargo is a real executable and stays on execFile without a shell.
  assert.deepEqual(execs[1], ["cargo", ["update", "-p", "oxc_resolver", "--precise", "11.22.0"]]);
});

test("updateOxcStack resolves latest versions before editing", async () => {
  const repo = await tempRepo();

  const result = await updateOxcStack({
    rootDir: repo.root,
    paths: repo.paths,
    dryRun: true,
    fetchJson: async (url) => {
      if (url.endsWith("/oxc_parser")) {
        return { crate: { max_stable_version: "0.136.0" }, versions: [] };
      }
      if (url.endsWith("/oxc_resolver")) {
        return { crate: { max_stable_version: "11.23.0" }, versions: [] };
      }
      return availableVersionPayload(url);
    },
  });

  assert.equal(result.oxcVersion, "0.136.0");
  assert.equal(result.resolverVersion, "11.23.0");
});

test("updateOxcStack reports no changed files when target versions and scripts already match", async () => {
  const manifest = manifestFixture();
  manifest.scripts = {
    "deps:update": "pnpm deps:update:oxc",
    "deps:update:oxc": "node scripts/update-oxc-stack.mjs",
    "deps:update:all": "pnpm update --latest && cargo update",
  };
  const repo = await tempRepo({ manifest });

  const result = await updateOxcStack({
    rootDir: repo.root,
    paths: repo.paths,
    dryRun: true,
    oxcVersion: "0.138.0",
    resolverVersion: "11.22.0",
    fetchJson: async (url) => {
      if (url.endsWith("/oxc_resolver/11.22.0")) {
        return { version: { num: "11.22.0" } };
      }
      const crate = oxcStackConfig.oxcCrates.find((crate) => url.endsWith(`/${crate}/0.138.0`));
      if (crate) {
        return { version: { num: "0.138.0" } };
      }
      throw new Error(`unexpected fetch: ${url}`);
    },
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
      resolverVersion: "11.22.0",
      fetchJson: availableVersions(),
    }),
    /Invalid OXC version/,
  );

  await assert.rejects(
    updateOxcStack({
      rootDir: repo.root,
      paths: repo.paths,
      oxcVersion: "0.999.0",
      resolverVersion: "11.22.0",
      fetchJson: availableVersions(),
    }),
    /Unavailable OXC crate/,
  );

  assert.equal(await readFile(path.join(repo.root, repo.paths.cargoToml), "utf8"), before);
});

test("updateOxcStack rejects non-coordinated current OXC crates and oxc_mangler before edits", async () => {
  const nonCoordinated = await tempRepo({
    cargoToml: cargoTomlFixture().replace('oxc_ast = "~0.138.0"', 'oxc_ast = "~0.137.0"'),
  });
  const withMangler = await tempRepo({
    cargoToml: `${cargoTomlFixture()}oxc_mangler = "~0.138.0"\n`,
  });

  await assert.rejects(
    updateOxcStack({
      rootDir: nonCoordinated.root,
      paths: nonCoordinated.paths,
      oxcVersion: "0.139.0",
      resolverVersion: "11.22.0",
      fetchJson: availableVersions(),
    }),
    /Current OXC crate versions are not coordinated/,
  );

  await assert.rejects(
    updateOxcStack({
      rootDir: withMangler.root,
      paths: withMangler.paths,
      oxcVersion: "0.139.0",
      resolverVersion: "11.22.0",
      fetchJson: availableVersions(),
    }),
    /oxc_mangler must not be present/,
  );
});

const availableVersions = () => async (url) => availableVersionPayload(url);

const availableVersionPayload = (url) => {
  if (url.endsWith("/oxc_parser")) {
    return {
      crate: { max_stable_version: "0.139.0" },
      versions: [{ num: "0.139.0" }, { num: "0.136.0" }],
    };
  }
  if (url.endsWith("/oxc_resolver")) {
    return {
      crate: { max_stable_version: "11.22.0" },
      versions: [{ num: "11.22.0" }, { num: "11.23.0" }],
    };
  }
  if (url.endsWith("/oxc_resolver/11.22.0")) {
    return { version: { num: "11.22.0" } };
  }
  if (url.endsWith("/oxc_resolver/11.23.0")) {
    return { version: { num: "11.23.0" } };
  }
  const crate = oxcStackConfig.oxcCrates.find(
    (crate) => url.endsWith(`/${crate}/0.139.0`) || url.endsWith(`/${crate}/0.136.0`),
  );
  if (crate) {
    return { version: { num: url.endsWith("0.136.0") ? "0.136.0" : "0.139.0" } };
  }
  throw new Error(`unexpected fetch: ${url}`);
};

const cargoTomlFixture = () => `[dependencies]
brotli = "^8"
${oxcStackConfig.oxcCrates.map((crate) => `${crate} = "~0.138.0"`).join("\n")}
oxc_resolver = "~11.22.0"
zstd = "^0.13"
`;

const manifestFixture = () => ({
  scripts: {
    "deps:update": "pnpm update --latest && cargo update",
  },
  dependencies: {
    "@msgpack/msgpack": "3.1.3",
  },
});

const dependencyPolicyFixture = () => `assert.match(cargoToml, /^oxc_parser = "~0\\.138\\.0"$/mu);
assert.match(cargoToml, /^oxc_resolver = "~11\\.22\\.0"$/mu);
assert.equal(manifest.dependencies["oxc-parser"], undefined);
`;

const packageVsixFixture =
  () => `const manifest = { dependencies: { "@msgpack/msgpack": "3.1.3" } };
assert.deepEqual(staged.dependencies, { "@msgpack/msgpack": "3.1.3" });
`;

const srsFixture = () => `| \`oxc_parser\`      | 0.138.0 |
| \`oxc_resolver\`    | 11.22.0 |
currently resolved to 0.138.0
Currently resolved to 11.22.0
baseline v0.138.0
`;

const configFixture = () => `export const oxcStackConfig = {
  currentOxcVersion: "0.138.0",
  currentResolverVersion: "11.22.0",
};
`;
