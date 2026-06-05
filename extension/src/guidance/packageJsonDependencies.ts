import { rangeFromOffsets } from "../imports/positions.js";
import type { SourceRange } from "../imports/types.js";

export interface PackageJsonDependencyEntry {
  name: string;
  version: string;
  range: SourceRange;
}

export const packageJsonDependencyEntries = (source: string): PackageJsonDependencyEntry[] => {
  let parsed: unknown;

  try {
    parsed = JSON.parse(source);
  } catch {
    return [];
  }

  if (!parsed || typeof parsed !== "object") {
    return [];
  }

  const root = parsed as Record<string, unknown>;
  const entries: PackageJsonDependencyEntry[] = [];

  for (const section of ["dependencies", "devDependencies", "peerDependencies", "optionalDependencies"]) {
    const dependencies = root[section];

    if (!dependencies || typeof dependencies !== "object") {
      continue;
    }

    const sectionOffset = source.indexOf(JSON.stringify(section));
    for (const [name, version] of Object.entries(dependencies as Record<string, unknown>)) {
      if (typeof version !== "string") {
        continue;
      }

      const nameOffset = source.indexOf(JSON.stringify(name), Math.max(0, sectionOffset));
      const endOffset = nameOffset >= 0 ? nameOffset + name.length + 2 : 0;
      entries.push({
        name,
        version,
        range: rangeFromOffsets(source, Math.max(0, nameOffset), endOffset),
      });
    }
  }

  return entries.sort((left, right) =>
    left.range.start.line - right.range.start.line
    || left.name.localeCompare(right.name));
};
