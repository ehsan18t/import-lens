import path from "node:path";
import { readFile } from "node:fs/promises";
import type { DetectedImport } from "./types.js";

export type ImportLensIgnoreRuleKind = "package" | "import" | "path";

export interface ImportLensIgnoreRule {
  kind: ImportLensIgnoreRuleKind;
  pattern: string;
}

export const parseImportLensIgnore = (contents: string): ImportLensIgnoreRule[] =>
  contents
    .split(/\r?\n/u)
    .map((line) => line.trim())
    .filter((line) => line.length > 0 && !line.startsWith("#"))
    .map((line) => {
      const separator = line.indexOf(":");
      const kind = separator > 0 ? line.slice(0, separator) : "import";
      const pattern = separator > 0 ? line.slice(separator + 1) : line;

      if (kind === "package" || kind === "import" || kind === "path") {
        return { kind, pattern };
      }

      return { kind: "import", pattern: line };
    });

export const shouldIgnoreImport = (
  detected: DetectedImport,
  sourceFile: string,
  rules: readonly ImportLensIgnoreRule[],
): boolean =>
  rules.some((rule) => {
    if (rule.kind === "package") {
      return globMatches(rule.pattern, detected.packageName);
    }

    if (rule.kind === "path") {
      return globMatchesPath(rule.pattern, sourceFile);
    }

    return globMatches(rule.pattern, detected.specifier);
  });

export const loadImportLensIgnore = async (startFilePath: string): Promise<ImportLensIgnoreRule[]> => {
  const ignorePath = await findImportLensIgnore(startFilePath);

  if (!ignorePath) {
    return [];
  }

  try {
    return parseImportLensIgnore(await readFile(ignorePath, "utf8"));
  } catch {
    return [];
  }
};

const findImportLensIgnore = async (startFilePath: string): Promise<string | null> => {
  let current = path.dirname(startFilePath);

  while (true) {
    const candidate = path.join(current, ".importlensignore");

    try {
      await readFile(candidate, "utf8");
      return candidate;
    } catch {
      const parent = path.dirname(current);
      if (parent === current) {
        return null;
      }
      current = parent;
    }
  }
};

const globMatchesPath = (pattern: string, filePath: string): boolean => {
  const normalizedPattern = normalizePath(pattern);
  const normalizedPath = normalizePath(filePath);
  return globRegex(normalizedPattern, true).test(normalizedPath);
};

const globMatches = (pattern: string, value: string): boolean =>
  globRegex(pattern, false).test(value);

const globRegex = (pattern: string, pathMode: boolean): RegExp => {
  const escaped = pattern
    .replace(/[.+^${}()|[\]\\]/gu, "\\$&")
    .replace(/\*\*/gu, "\0")
    .replace(/\*/gu, "[^/]*")
    .replace(/\0/gu, ".*");
  const prefix = pathMode && !pattern.startsWith("/") ? "(?:^|.*/)" : "^";
  return new RegExp(`${prefix}${escaped}$`, "u");
};

const normalizePath = (value: string): string =>
  value.split(path.sep).join("/");
