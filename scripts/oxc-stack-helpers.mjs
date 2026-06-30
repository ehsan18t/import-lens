import { oxcStackConfig } from "./oxc-stack.config.mjs";

const semverPattern = /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/u;

export const validateCurrentStack = (cargoToml, _manifest) => {
  if (/^oxc_mangler\s*=/mu.test(cargoToml)) {
    throw new Error("oxc_mangler must not be present in daemon/Cargo.toml");
  }

  const crateVersions = oxcStackConfig.oxcCrates.map((crate) => {
    const match = cargoToml.match(new RegExp(`^${crate}\\s*=\\s*"(=[^"]+)"$`, "mu"));
    if (!match) {
      throw new Error(`Missing exact OXC crate pin: ${crate}`);
    }
    return match[1].slice(1);
  });
  const uniqueCrateVersions = new Set(crateVersions);
  if (uniqueCrateVersions.size !== 1) {
    throw new Error(`Current OXC crate versions are not coordinated: ${[...uniqueCrateVersions].join(", ")}`);
  }

  if (!/^oxc_resolver\s*=\s*"=[^"]+"$/mu.test(cargoToml)) {
    throw new Error("Missing exact oxc_resolver pin");
  }
};

export const validateVersion = (label, version) => {
  if (!semverPattern.test(version)) {
    throw new Error(`Invalid ${label} version: ${version}`);
  }
};

export const validateAvailableVersions = async (fetchJson, oxcVersion, resolverVersion) => {
  await Promise.all(
    oxcStackConfig.oxcCrates.map((crate) =>
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

export const updateCargoToml = (cargoToml, oxcVersion, resolverVersion) => {
  let next = cargoToml;
  for (const crate of oxcStackConfig.oxcCrates) {
    next = next.replace(new RegExp(`^${crate}\\s*=\\s*"[^"]+"$`, "gmu"), `${crate} = "=${oxcVersion}"`);
  }
  return next.replace(/^oxc_resolver\s*=\s*"[^"]+"$/gmu, `oxc_resolver = "=${resolverVersion}"`);
};

export const updateManifest = (manifest, oxcVersion) => {
  const next = structuredClone(manifest);
  void oxcVersion;

  next.scripts = {
    ...(next.scripts ?? {}),
    "deps:update": "pnpm deps:update:oxc",
    "deps:update:oxc": "node scripts/update-oxc-stack.mjs",
    "deps:update:all": "pnpm update --latest && cargo update",
  };

  return `${JSON.stringify(next, null, 2)}\n`;
};

export const replaceKnownVersions = (content, oxcVersion, resolverVersion) =>
  content
    .replaceAll(oxcStackConfig.currentOxcVersion, oxcVersion)
    .replaceAll(oxcStackConfig.currentResolverVersion, resolverVersion);

export const updateConfig = (content, oxcVersion, resolverVersion) =>
  content
    .replace(
      /currentOxcVersion:\s*"[^"]+"/u,
      `currentOxcVersion: "${oxcVersion}"`,
    )
    .replace(
      /currentResolverVersion:\s*"[^"]+"/u,
      `currentResolverVersion: "${resolverVersion}"`,
    );

export const formatOxcUpdateResult = ({ dryRun, oxcVersion, resolverVersion, changedFiles }) => {
  const mode = dryRun ? "Dry run" : "Updated";
  const files = changedFiles.length === 0 ? "No file edits needed." : `Files: ${changedFiles.join(", ")}`;
  return `${mode}: OXC ${oxcVersion}, oxc_resolver ${resolverVersion}\n${files}\n`;
};

const crateVersion = async (fetchJson, crate, version) => {
  const payload = await fetchJson(`https://crates.io/api/v1/crates/${crate}/${version}`);
  const returnedVersion = payload?.version?.num;
  if (returnedVersion !== version) {
    throw new Error(`crates.io returned ${returnedVersion ?? "no version"}`);
  }
};
