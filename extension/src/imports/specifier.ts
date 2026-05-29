import { builtinModules } from "node:module";

const nodeBuiltinSpecifiers = new Set<string>([
  ...builtinModules,
  ...builtinModules.map((moduleName) => moduleName.replace(/^node:/, "")),
]);

export const getPackageName = (specifier: string): string => {
  if (specifier.startsWith("@")) {
    const [scope, name] = specifier.split("/");
    return scope && name ? `${scope}/${name}` : specifier;
  }

  return specifier.split("/")[0] ?? specifier;
};

export const isRelativeSpecifier = (specifier: string): boolean =>
  specifier.startsWith("./") ||
  specifier.startsWith("../") ||
  specifier.startsWith("/") ||
  specifier.startsWith(".\\") ||
  specifier.startsWith("..\\");

export const isNodeBuiltinSpecifier = (specifier: string): boolean => {
  const normalized = specifier.startsWith("node:") ? specifier.slice("node:".length) : specifier;
  return nodeBuiltinSpecifiers.has(normalized);
};

export const isRuntimePackageSpecifier = (specifier: string): boolean =>
  !isRelativeSpecifier(specifier) && !isNodeBuiltinSpecifier(specifier);

