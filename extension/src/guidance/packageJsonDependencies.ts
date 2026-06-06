import { rangeFromOffsets } from "../imports/positions.js";
import type { SourceRange } from "../imports/types.js";

export type PackageJsonDependencySectionName =
  | "dependencies"
  | "devDependencies"
  | "peerDependencies"
  | "optionalDependencies";

export interface PackageJsonDependencyEntry {
  name: string;
  version: string;
  section: PackageJsonDependencySectionName;
  range: SourceRange;
  nameRange: SourceRange;
  valueRange: SourceRange;
}

export interface PackageJsonDependencySection {
  section: PackageJsonDependencySectionName;
  range: SourceRange;
  objectRange: SourceRange;
}

const dependencySectionNames = [
  "dependencies",
  "devDependencies",
  "peerDependencies",
  "optionalDependencies",
] as const satisfies readonly PackageJsonDependencySectionName[];

const dependencySectionNameSet = new Set<string>(dependencySectionNames);

interface JsonStringToken {
  value: string;
  start: number;
  end: number;
}

interface DependencySectionObject {
  section: PackageJsonDependencySectionName;
  keyStart: number;
  keyEnd: number;
  objectStart: number;
  objectEnd: number;
}

const skipWhitespace = (source: string, offset: number): number => {
  let current = offset;

  while (current < source.length && /\s/u.test(source[current] ?? "")) {
    current++;
  }

  return current;
};

const readJsonString = (source: string, start: number): JsonStringToken | null => {
  if (source[start] !== "\"") {
    return null;
  }

  let current = start + 1;

  while (current < source.length) {
    const char = source[current];

    if (char === "\\") {
      current += 2;
      continue;
    }

    if (char === "\"") {
      const end = current + 1;

      try {
        return {
          value: JSON.parse(source.slice(start, end)) as string,
          start,
          end,
        };
      } catch {
        return null;
      }
    }

    current++;
  }

  return null;
};

const matchingObjectEnd = (source: string, objectStart: number): number => {
  let depth = 0;
  let current = objectStart;

  while (current < source.length) {
    const char = source[current];

    if (char === "\"") {
      const token = readJsonString(source, current);

      if (!token) {
        return -1;
      }

      current = token.end;
      continue;
    }

    if (char === "{") {
      depth++;
    } else if (char === "}") {
      depth--;

      if (depth === 0) {
        return current + 1;
      }
    }

    current++;
  }

  return -1;
};

const dependencySectionObjects = (source: string): DependencySectionObject[] => {
  const sections: DependencySectionObject[] = [];
  let depth = 0;
  let current = 0;

  while (current < source.length) {
    const char = source[current];

    if (char === "\"") {
      const token = readJsonString(source, current);

      if (!token) {
        return sections;
      }

      const colonOffset = skipWhitespace(source, token.end);
      if (depth === 1 && source[colonOffset] === ":" && dependencySectionNameSet.has(token.value)) {
        const objectStart = skipWhitespace(source, colonOffset + 1);
        const objectEnd = source[objectStart] === "{" ? matchingObjectEnd(source, objectStart) : -1;

        if (objectEnd >= 0) {
          sections.push({
            section: token.value as PackageJsonDependencySectionName,
            keyStart: token.start,
            keyEnd: token.end,
            objectStart,
            objectEnd,
          });
        }
      }

      current = token.end;
      continue;
    }

    if (char === "{" || char === "[") {
      depth++;
    } else if (char === "}" || char === "]") {
      depth--;
    }

    current++;
  }

  return sections;
};

const dependencyEntriesForSection = (
  source: string,
  section: DependencySectionObject,
): PackageJsonDependencyEntry[] => {
  const entries: PackageJsonDependencyEntry[] = [];
  let depth = 0;
  let current = section.objectStart;

  while (current < section.objectEnd) {
    const char = source[current];

    if (char === "\"") {
      const nameToken = readJsonString(source, current);

      if (!nameToken) {
        return entries;
      }

      const colonOffset = skipWhitespace(source, nameToken.end);
      const valueStart = skipWhitespace(source, colonOffset + 1);
      const valueToken = source[colonOffset] === ":" ? readJsonString(source, valueStart) : null;

      if (depth === 1 && valueToken) {
        const nameRange = rangeFromOffsets(source, nameToken.start, nameToken.end);

        entries.push({
          name: nameToken.value,
          version: valueToken.value,
          section: section.section,
          range: nameRange,
          nameRange,
          valueRange: rangeFromOffsets(source, valueToken.start, valueToken.end),
        });
      }

      current = nameToken.end;
      continue;
    }

    if (char === "{" || char === "[") {
      depth++;
    } else if (char === "}" || char === "]") {
      depth--;
    }

    current++;
  }

  return entries;
};

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

  for (const section of dependencySectionObjects(source)) {
    const dependencies = root[section.section];

    if (!dependencies || typeof dependencies !== "object") {
      continue;
    }

    entries.push(...dependencyEntriesForSection(source, section));
  }

  return entries.sort((left, right) =>
    left.range.start.line - right.range.start.line
    || left.name.localeCompare(right.name));
};

export const packageJsonDependencySections = (source: string): PackageJsonDependencySection[] => {
  try {
    JSON.parse(source);
  } catch {
    return [];
  }

  return dependencySectionObjects(source).map((section) => ({
    section: section.section,
    range: rangeFromOffsets(source, section.keyStart, section.keyEnd),
    objectRange: rangeFromOffsets(source, section.objectStart, section.objectEnd),
  }));
};
