import type {
  ImportResult,
  PackageJsonDependencySectionName,
  RegistryHint,
} from "../ipc/protocol.js";

export type PackageJsonDependencyHintStatus = "loading" | "ready" | "missing" | "unavailable";

export type RegistryHintRefreshStatus = "fresh" | "stale";

export interface PackageJsonDependencyHintState {
  name: string;
  section: PackageJsonDependencySectionName;
  status: PackageJsonDependencyHintStatus;
  installedVersion?: string;
  result?: ImportResult;
  registryHint?: RegistryHint | null;
  registryHintRefreshStatus?: RegistryHintRefreshStatus;
  registryHintRefreshError?: string | null;
}
