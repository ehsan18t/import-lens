import { daemonRoot, extensionBundle } from "./targets.mjs";

export const createStagedManifest = ({ manifest }) => {
  const files = [
    extensionBundle,
    `${daemonRoot}/`,
    "cli/",
    "README.md",
    "LICENSE",
    "package.json",
  ];

  if (manifest.icon) {
    files.push(manifest.icon);
  }

  return {
    ...manifest,
    devDependencies: undefined,
    files,
    scripts: undefined,
  };
};
