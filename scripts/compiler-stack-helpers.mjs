import { compilerStackConfig } from "./compiler-stack.config.mjs";

const semverPattern = /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/u;

const exactPin = (crate, cargoToml) => {
  const match = cargoToml.match(new RegExp(`^${crate}\\s*=\\s*"(=[^"]+)"$`, "mu"));
  return match?.[1].slice(1);
};

export const validateCurrentStack = (cargoToml) => {
  if (/^oxc_mangler\s*=/mu.test(cargoToml)) {
    throw new Error("oxc_mangler must not be present in daemon/Cargo.toml");
  }

  const crateVersions = compilerStackConfig.oxcCrates.map((crate) => {
    const version = exactPin(crate, cargoToml);
    if (!version) {
      throw new Error(`Missing exact pin (=) for OXC crate: ${crate}`);
    }
    return version;
  });
  const uniqueCrateVersions = new Set(crateVersions);
  if (uniqueCrateVersions.size !== 1) {
    throw new Error(
      `Current OXC crate versions are not coordinated: ${[...uniqueCrateVersions].join(", ")}`,
    );
  }

  if (!exactPin("oxc_resolver", cargoToml)) {
    throw new Error("Missing exact pin (=) for oxc_resolver");
  }

  if (
    !/^rolldown\s*=\s*\{\s*version\s*=\s*"=[^"]+",\s*optional\s*=\s*true\s*\}$/mu.test(cargoToml)
  ) {
    throw new Error(
      'Missing exact optional rolldown dependency (rolldown = { version = "=x.y.z", optional = true })',
    );
  }
  if (!/^rolldown-candidate\s*=\s*\[\s*"dep:rolldown"\s*\]$/mu.test(cargoToml)) {
    throw new Error(
      'Missing rolldown-candidate feature ([features] rolldown-candidate = ["dep:rolldown"])',
    );
  }
};

export const validateVersion = (label, version) => {
  if (!semverPattern.test(version)) {
    throw new Error(`Invalid ${label} version: ${version}`);
  }
};

export const validateAvailableVersions = async (
  fetchJson,
  { rolldownVersion, oxcVersion, resolverVersion },
) => {
  await crateVersion(fetchJson, compilerStackConfig.rolldownCrate, rolldownVersion).catch(
    (error) => {
      throw new Error(`Unavailable rolldown version ${rolldownVersion}: ${error.message}`);
    },
  );

  // Cargo's probe resolution proves the umbrella graph, but not that every
  // retained DIRECT crate exists at the monorepo version -- the umbrella does
  // not depend on all of them.
  await Promise.all(
    compilerStackConfig.oxcCrates.map((crate) =>
      crateVersion(fetchJson, crate, oxcVersion).catch((error) => {
        throw new Error(`Unavailable OXC crate ${crate}@${oxcVersion}: ${error.message}`);
      }),
    ),
  );

  await crateVersion(fetchJson, "oxc_resolver", resolverVersion).catch((error) => {
    throw new Error(`Unavailable oxc_resolver version ${resolverVersion}: ${error.message}`);
  });
};

export const latestCrateVersion = async (fetchJson, crate) => {
  const payload = await fetchJson(`https://crates.io/api/v1/crates/${crate}`);
  const version = payload?.crate?.max_stable_version ?? payload?.crate?.newest_version;
  if (!version) {
    throw new Error(`Could not resolve latest crate version for ${crate}`);
  }
  return version;
};

export const updateCargoToml = (cargoToml, { rolldownVersion, oxcVersion, resolverVersion }) => {
  let next = cargoToml;
  for (const crate of compilerStackConfig.oxcCrates) {
    next = next.replace(
      new RegExp(`^${crate}\\s*=\\s*"[^"]+"$`, "gmu"),
      `${crate} = "=${oxcVersion}"`,
    );
  }
  next = next.replace(/^oxc_resolver\s*=\s*"[^"]+"$/gmu, `oxc_resolver = "=${resolverVersion}"`);
  return next.replace(
    /^rolldown\s*=\s*\{\s*version\s*=\s*"[^"]+",\s*optional\s*=\s*true\s*\}$/gmu,
    `rolldown = { version = "=${rolldownVersion}", optional = true }`,
  );
};

export const updateManifest = (manifest) => {
  const next = structuredClone(manifest);

  next.scripts = {
    ...(next.scripts ?? {}),
    "deps:update:compiler": "node scripts/update-compiler-stack.mjs",
    // General refresh stays range-respecting, but success now requires the
    // recorded compiler stack to survive it, so it is a script that restores
    // and validates rather than a bare `pnpm update && cargo update` chain.
    "deps:update:safe": "node scripts/deps-update-safe.mjs",
  };

  return `${JSON.stringify(next, null, 2)}\n`;
};

const escapeRegExp = (value) => value.replace(/[.*+?^${}()|[\]\\]/gu, "\\$&");

export const replaceKnownVersions = (content, { rolldownVersion, oxcVersion, resolverVersion }) => {
  // Replace all three pinned versions in a single word-boundary pass. Word
  // boundaries keep a version that is only a substring of a longer number (a
  // build id, an unrelated figure) intact, and a single pass means replacing
  // one version can never corrupt another's needle - a hazard of chained
  // replaceAll if one version string ever contained another.
  const replacements = new Map([
    [compilerStackConfig.currentRolldownVersion, rolldownVersion],
    [compilerStackConfig.currentOxcVersion, oxcVersion],
    [compilerStackConfig.currentResolverVersion, resolverVersion],
  ]);
  const needles = [...replacements.keys()]
    .sort((left, right) => right.length - left.length)
    .map(escapeRegExp);
  const pattern = new RegExp(`\\b(?:${needles.join("|")})\\b`, "gu");

  return content.replace(pattern, (match) => replacements.get(match) ?? match);
};

export const updateConfig = (content, { rolldownVersion, oxcVersion, resolverVersion }) =>
  content
    .replace(/currentRolldownVersion:\s*"[^"]+"/u, `currentRolldownVersion: "${rolldownVersion}"`)
    .replace(/currentOxcVersion:\s*"[^"]+"/u, `currentOxcVersion: "${oxcVersion}"`)
    .replace(/currentResolverVersion:\s*"[^"]+"/u, `currentResolverVersion: "${resolverVersion}"`);

export const formatCompilerUpdateResult = ({
  dryRun,
  rolldownVersion,
  oxcVersion,
  resolverVersion,
  changedFiles,
}) => {
  const mode = dryRun ? "Dry run" : "Updated";
  const files =
    changedFiles.length === 0 ? "No file edits needed." : `Files: ${changedFiles.join(", ")}`;
  return `${mode}: rolldown ${rolldownVersion}, OXC ${oxcVersion}, oxc_resolver ${resolverVersion}\n${files}\n`;
};

const crateVersion = async (fetchJson, crate, version) => {
  const payload = await fetchJson(`https://crates.io/api/v1/crates/${crate}/${version}`);
  const returnedVersion = payload?.version?.num;
  if (returnedVersion !== version) {
    throw new Error(`crates.io returned ${returnedVersion ?? "no version"}`);
  }
};
