import type { CacheStatusResponse } from "../ipc/protocol.js";

export type CacheManagerAction =
  | "summary"
  | "clearCurrent"
  | "clearAllProjects"
  | "clearOrphans"
  | "clearRegistry"
  | "clearEverything";

export interface CacheManagerActionItem {
  label: string;
  description?: string;
  detail?: string;
  action: CacheManagerAction;
}

/**
 * The four scoped clears, mapped to their real daemon `CacheRemoveScope`:
 * `currentProject` -> CurrentProject, `allProjects` -> Selected (every shard id;
 * no dedicated all-shards scope exists, so registry + resolvers survive),
 * `registry` -> Registry, `everything` -> All (shards + registry + resolvers +
 * derived caches). Used to keep the success toast honest per scope.
 */
export type CacheClearScope =
  | "currentProject"
  | "allProjects"
  | "orphans"
  | "registry"
  | "everything";

export const formatCacheBytes = (bytes: number): string => {
  if (bytes < 1024) {
    return `${bytes} B`;
  }

  const kilobytes = bytes / 1024;

  if (kilobytes < 1024) {
    return `${Math.round(kilobytes)} kB`;
  }

  const megabytes = kilobytes / 1024;

  if (megabytes < 1024) {
    return `${Math.round(megabytes)} MB`;
  }

  return `${(megabytes / 1024).toFixed(1)} GB`;
};

const entriesLabel = (count: number): string => `${count} entr${count === 1 ? "y" : "ies"}`;

const projectsLabel = (count: number): string => `${count} project${count === 1 ? "" : "s"}`;

// Coarse relative age for the read-only "last used" row. `now` is injected so the
// rendering stays deterministic in tests.
const formatLastUsedAge = (millis: number, now: number): string => {
  const deltaMs = Math.max(0, now - millis);
  const days = Math.floor(deltaMs / 86_400_000);
  if (days >= 1) {
    return `${days}d ago`;
  }
  const hours = Math.floor(deltaMs / 3_600_000);
  if (hours >= 1) {
    return `${hours}h ago`;
  }
  const minutes = Math.floor(deltaMs / 60_000);
  if (minutes >= 1) {
    return `${minutes}m ago`;
  }
  return "just now";
};

/**
 * Builds the Manage-Cache quick-pick: two read-only status rows (E-status
 * observability — total vs budget, headroom, registry size, per-project
 * size/entry-count/last-used) followed by the four scoped clear actions (§8).
 */
export const cacheManagerActionItems = (
  status: CacheStatusResponse,
  now: number = Date.now(),
): CacheManagerActionItem[] => {
  // Prefer the E-status fields; fall back so a response from a daemon that
  // predates them still renders (`?? ...`).
  const totalBytes = status.total_bytes ?? status.total_size_bytes;
  const budgetBytes = status.budget_bytes ?? status.max_size_mb * 1024 * 1024;
  const registryBytes = status.registry_size_bytes ?? 0;
  const headroomBytes = Math.max(0, budgetBytes - totalBytes);
  const projects = projectsLabel(status.project_count);

  const current = status.current_project;
  const currentDescription = current
    ? `${formatCacheBytes(current.size_bytes)} - ${entriesLabel(current.entry_count ?? 0)}`
    : "Not cached yet";
  const currentDetail = current
    ? current.last_used_millis != null
      ? `Last used ${formatLastUsedAge(current.last_used_millis, now)}`
      : "Never used"
    : undefined;

  return [
    {
      label: "$(database) Import Lens cache",
      description: `${formatCacheBytes(totalBytes)} / ${formatCacheBytes(budgetBytes)}`,
      detail: `${formatCacheBytes(headroomBytes)} free - ${projects} - registry ${formatCacheBytes(registryBytes)}`,
      action: "summary",
    },
    {
      label: "$(folder) Current project",
      description: currentDescription,
      detail: currentDetail,
      action: "summary",
    },
    {
      label: "$(trash) Clear current project",
      description: current ? formatCacheBytes(current.size_bytes) : "No cache yet",
      action: "clearCurrent",
    },
    {
      label: "$(trash) Clear all projects",
      description: `${projects} - bundle caches (keeps registry)`,
      action: "clearAllProjects",
    },
    {
      label: "$(history) Remove orphaned caches",
      description: "Reclaim caches for moved or deleted projects",
      action: "clearOrphans",
    },
    {
      label: "$(trash) Clear registry metadata",
      description: `npm hints - ${formatCacheBytes(registryBytes)}`,
      action: "clearRegistry",
    },
    {
      label: "$(clear-all) Clear everything",
      description: "All project caches + registry + derived state",
      action: "clearEverything",
    },
  ];
};

/**
 * Honest success/partial-failure copy for a scoped clear (X-23/X-24): counts
 * come from `CacheRemoveResponse`, the registry-only scope never claims shard
 * removals, and "everything" states everything it cleared — no overclaiming.
 */
export const cacheRemovalToast = (
  scope: CacheClearScope,
  removed: number,
  failed: number,
): string => {
  if (scope === "registry") {
    // Registry clear returns no shard-level results; report exactly what it did.
    return "Cleared Import Lens registry metadata (npm hints).";
  }

  const caches = (count: number): string => `${count} Import Lens cache${count === 1 ? "" : "s"}`;

  if (scope === "orphans") {
    if (failed > 0) {
      return `Reclaimed ${caches(removed)} for moved or deleted projects; ${failed} could not be removed.`;
    }
    return removed > 0
      ? `Reclaimed ${caches(removed)} for moved or deleted projects.`
      : "No orphaned Import Lens caches to reclaim.";
  }

  if (failed > 0) {
    const removedClause =
      scope === "currentProject"
        ? `Removed ${caches(removed)} for the current project`
        : scope === "allProjects"
          ? `Removed ${caches(removed)} across all projects`
          : `Removed ${caches(removed)} plus registry metadata and derived caches`;
    return `${removedClause}; ${failed} could not be removed.`;
  }

  if (scope === "currentProject") {
    return removed > 0
      ? "Cleared the current project's Import Lens cache."
      : "No Import Lens cache to clear for the current project.";
  }
  if (scope === "allProjects") {
    return `Cleared ${caches(removed)} across all projects.`;
  }
  return `Cleared everything: ${caches(removed)} plus registry metadata and derived caches.`;
};
