import path from "node:path";

export const createStagedManifest = ({ manifest, bindingPackage }) => {
  const files = [
    "extension/dist/extension.cjs",
    "bin/",
    "cli/",
    "node_modules/oxc-parser/",
    `node_modules/@oxc-parser/${path.basename(bindingPackage)}/`,
    "node_modules/@oxc-project/types/",
    "README.md",
    "LICENSE",
    "package.json",
  ];

  if (manifest.icon) {
    files.push(manifest.icon);
  }

  return {
    ...manifest,
    dependencies: {
      [bindingPackage]: manifest.dependencies[bindingPackage] ?? manifest.dependencies["oxc-parser"],
      "oxc-parser": manifest.dependencies["oxc-parser"],
    },
    devDependencies: undefined,
    files,
    scripts: undefined,
  };
};
