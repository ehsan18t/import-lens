import {
  ExportImportNameKind,
  ImportNameKind,
  parseSync,
  type ParserOptions,
  type StaticExportEntry,
  type StaticImport,
  type StaticImportEntry,
} from "oxc-parser";
import { getPackageName, isRuntimePackageSpecifier } from "./specifier.js";
import type { DetectedImport } from "./types.js";
import { positionAt, rangeFromOffsets } from "./positions.js";

const parserOptions: ParserOptions & { recovery?: boolean } = {
  sourceType: "module",
  astType: "ts",
  range: false,
  recovery: true,
};

const languageFromFilename = (filename: string): ParserOptions["lang"] => {
  if (filename.endsWith(".tsx")) {
    return "tsx";
  }

  if (filename.endsWith(".ts")) {
    return "ts";
  }

  if (filename.endsWith(".jsx")) {
    return "jsx";
  }

  return "js";
};

const trimLiteralQuotes = (value: string): string => value.replace(/^['"`]|['"`]$/gu, "");

const createDetectedImport = (
  source: string,
  specifier: string,
  importKind: DetectedImport["importKind"],
  named: string[],
  start: number,
  end: number,
  quoteEndOffset: number,
): DetectedImport => ({
  specifier,
  packageName: getPackageName(specifier),
  named: [...named].sort(),
  importKind,
  line: positionAt(source, start).line,
  quoteEnd: positionAt(source, quoteEndOffset),
  statementRange: rangeFromOffsets(source, start, end),
});

const runtimeEntries = (entries: StaticImportEntry[]): StaticImportEntry[] =>
  entries.filter((entry) => !entry.isType);

const importsFromStaticImport = (source: string, item: StaticImport): DetectedImport[] => {
  const specifier = item.moduleRequest.value;

  if (!isRuntimePackageSpecifier(specifier)) {
    return [];
  }

  const entries = runtimeEntries(item.entries);

  if (entries.length === 0 && item.entries.length === 0) {
    return [
      createDetectedImport(source, specifier, "namespace", [], item.start, item.end, item.moduleRequest.end),
    ];
  }

  if (entries.length === 0) {
    return [];
  }

  const imports: DetectedImport[] = [];
  const named = entries
    .filter((entry) => entry.importName.kind === ImportNameKind.Name && entry.importName.name)
    .map((entry) => entry.importName.name as string);

  if (entries.some((entry) => entry.importName.kind === ImportNameKind.Default)) {
    imports.push(createDetectedImport(source, specifier, "default", [], item.start, item.end, item.moduleRequest.end));
  }

  if (entries.some((entry) => entry.importName.kind === ImportNameKind.NamespaceObject)) {
    imports.push(createDetectedImport(source, specifier, "namespace", [], item.start, item.end, item.moduleRequest.end));
  }

  if (named.length > 0) {
    imports.push(createDetectedImport(source, specifier, "named", named, item.start, item.end, item.moduleRequest.end));
  }

  return imports;
};

const importFromStaticExport = (source: string, entry: StaticExportEntry, statementStart: number, statementEnd: number): DetectedImport | null => {
  if (entry.isType || !entry.moduleRequest) {
    return null;
  }

  const specifier = entry.moduleRequest.value;

  if (!isRuntimePackageSpecifier(specifier)) {
    return null;
  }

  if (entry.importName.kind === ExportImportNameKind.All || entry.importName.kind === ExportImportNameKind.AllButDefault) {
    return createDetectedImport(source, specifier, "namespace", [], statementStart, statementEnd, entry.moduleRequest.end);
  }

  if (entry.importName.kind === ExportImportNameKind.Name && entry.importName.name) {
    return createDetectedImport(source, specifier, "named", [entry.importName.name], statementStart, statementEnd, entry.moduleRequest.end);
  }

  return null;
};

export const extractRuntimeImports = (filename: string, source: string): DetectedImport[] => {
  const parsed = parseSync(filename, source, {
    ...parserOptions,
    lang: languageFromFilename(filename),
  });
  const imports: DetectedImport[] = [];

  for (const item of parsed.module.staticImports) {
    imports.push(...importsFromStaticImport(source, item));
  }

  for (const item of parsed.module.staticExports) {
    for (const entry of item.entries) {
      const detected = importFromStaticExport(source, entry, item.start, item.end);

      if (detected) {
        imports.push(detected);
      }
    }
  }

  for (const item of parsed.module.dynamicImports) {
    const specifier = trimLiteralQuotes(source.slice(item.moduleRequest.start, item.moduleRequest.end));

    if (specifier && isRuntimePackageSpecifier(specifier)) {
      imports.push(createDetectedImport(source, specifier, "dynamic", [], item.start, item.end, item.moduleRequest.end));
    }
  }

  return imports.sort((left, right) => left.statementRange.start.line - right.statementRange.start.line);
};
