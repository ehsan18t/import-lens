import { readFile } from "node:fs/promises";
import path from "node:path";
import type { PackageJsonDependencyEntry } from "../ipc/protocol.js";
import type { PackageJsonDependencyHintState } from "./packageJsonState.js";

export type RegistryTargetState = PackageJsonDependencyHintState & {
  entry: Pick<PackageJsonDependencyEntry, "version">;
};

const publicRegistryHosts = new Set(["registry.npmjs.org", "registry.npmjs.com"]);
const scopeRegistryLine = /^(@[^\s:=]+):registry\s*=\s*(.*)$/u;

/**
 * Whether a dependency's registry hint may be looked up on the public npm registry. The daemon
 * queries registry.npmjs.org by the dependency KEY, so anything else would leak a private name to
 * the public registry and report on an unrelated package that happens to share it:
 *
 * - a spec with `:` or `/` is a protocol, path, URL, git shorthand or alias (`workspace:*`,
 *   `file:`, `link:`, `portal:`, `github:`, `https:`, `owner/repo`, `jsr:`, `npm:bar@1`), except
 *   pnpm's `catalog:`, which resolves to a registry range;
 * - a scoped name whose scope an `.npmrc` maps to another registry.
 */
export const isPublicRegistryDependency = (
  name: string,
  spec: string,
  privateScopes: ReadonlySet<string>,
): boolean => {
  const trimmed = spec.trim();

  if (!trimmed.startsWith("catalog:") && (trimmed.includes(":") || trimmed.includes("/"))) {
    return false;
  }

  if (!name.startsWith("@")) {
    return true;
  }

  const slash = name.indexOf("/");

  return !privateScopes.has(slash === -1 ? name : name.slice(0, slash));
};

/**
 * Clears a registry hint the daemon served from its cache for a dependency that is not on the
 * public registry: such a hint describes an unrelated public package that shares the key.
 */
export const withoutNonPublicRegistryHints = <TState extends RegistryTargetState>(
  states: TState[],
  privateScopes: ReadonlySet<string>,
): TState[] =>
  states.map((state) =>
    state.registryHint &&
    !isPublicRegistryDependency(state.name, state.entry.version, privateScopes)
      ? { ...state, registryHint: null }
      : state,
  );

/**
 * The scopes that `.npmrc` texts map to a registry other than the public one. Texts are given in
 * increasing precedence (user file first, project file last), so a later file can remap a scope
 * back to the public registry.
 */
export const privateRegistryScopes = (npmrcTexts: readonly string[]): Set<string> => {
  const registries = new Map<string, string>();

  for (const text of npmrcTexts) {
    for (const rawLine of text.split(/\r?\n/u)) {
      const match = scopeRegistryLine.exec(rawLine.trim());

      if (match?.[1] && match[2] !== undefined) {
        registries.set(match[1], unquote(match[2].trim()));
      }
    }
  }

  return new Set(
    [...registries].filter(([, url]) => !isPublicRegistryUrl(url)).map(([scope]) => scope),
  );
};

/**
 * Reads `.npmrc` from each directory (lowest precedence first). A missing or unreadable file
 * contributes nothing.
 */
export const readPrivateRegistryScopes = async (
  directories: readonly string[],
): Promise<Set<string>> => {
  const unique = [...new Set(directories.map((directory) => path.resolve(directory)))];
  const texts = await Promise.all(
    unique.map((directory) =>
      readFile(path.join(directory, ".npmrc"), "utf8").catch((): string => ""),
    ),
  );

  return privateRegistryScopes(texts);
};

const unquote = (value: string): string =>
  value.length >= 2 &&
  (value.startsWith('"') || value.startsWith("'")) &&
  value.endsWith(value.charAt(0))
    ? value.slice(1, -1)
    : value;

const isPublicRegistryUrl = (value: string): boolean => {
  try {
    return publicRegistryHosts.has(new URL(value).hostname.toLowerCase());
  } catch {
    return false;
  }
};
