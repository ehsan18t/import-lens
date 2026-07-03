export const createStagedManifest = ({ manifest }) => {
  const files = [
    "extension/dist/extension.cjs",
    "bin/",
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
