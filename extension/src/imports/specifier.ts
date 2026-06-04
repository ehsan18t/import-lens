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
  specifier.startsWith("\\\\") ||
  specifier.startsWith(".\\") ||
  specifier.startsWith("..\\") ||
  /^[A-Za-z]:[\\/]/u.test(specifier);

const isUrlLikeSpecifier = (specifier: string): boolean =>
  /^[A-Za-z][A-Za-z\d+.-]*:/u.test(specifier);

export const isNodeBuiltinSpecifier = (specifier: string): boolean => {
  const normalized = specifier.startsWith("node:") ? specifier.slice("node:".length) : specifier;
  return nodeBuiltinSpecifiers.has(normalized);
};

const isFrameworkVirtualSpecifier = (specifier: string): boolean =>
  specifier.startsWith("astro:") ||
  specifier.startsWith("virtual:") ||
  specifier.startsWith("$") ||
  specifier.startsWith("#") ||
  specifier.startsWith("@/") ||
  specifier.startsWith("~/");

const hostProvidedModules: ReadonlySet<string> = new Set([
  "vscode",
  "electron",
]);

const isHostProvidedModule = (specifier: string): boolean =>
  hostProvidedModules.has(specifier) ||
  specifier.startsWith("bun:");

export const isRuntimePackageSpecifier = (specifier: string): boolean =>
  !isRelativeSpecifier(specifier) &&
  !isNodeBuiltinSpecifier(specifier) &&
  !isUrlLikeSpecifier(specifier) &&
  !isFrameworkVirtualSpecifier(specifier) &&
  !isHostProvidedModule(specifier);
