import type { ImportResult } from "../ipc/protocol.js";
import type { PackageJsonDependencySectionName } from "./packageJsonDependencies.js";
import type { RegistryHint } from "./registryHints.js";

export type PackageJsonDependencyHintStatus = "loading" | "ready" | "missing" | "unavailable";

export interface PackageJsonDependencyHintState {
  name: string;
  section: PackageJsonDependencySectionName;
  status: PackageJsonDependencyHintStatus;
  installedVersion?: string;
  result?: ImportResult;
  registryHint?: RegistryHint | null;
}
