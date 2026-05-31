import {
  ExportImportNameKind,
  ImportNameKind,
  parseSync,
  type ParserOptions,
  type StaticExport,
  type StaticImport,
  type StaticImportEntry,
} from "oxc-parser";
import { getPackageName, isRuntimePackageSpecifier } from "./specifier.js";
import { scriptRegionsForDocument, type ScriptRegion } from "./scriptRegions.js";
import type { DetectedImport } from "./types.js";
import { positionAt, rangeFromOffsets } from "./positions.js";

const parserOptions: ParserOptions & { recovery?: boolean } = {
  sourceType: "module",
  astType: "ts",
  range: false,
  recovery: true,
};

const literalDynamicImportSpecifier = (value: string): string | null => {
  const first = value.at(0);
  const last = value.at(-1);

  if ((first === "'" || first === '"') && first === last) {
    return value.slice(1, -1);
  }

  if (first === "`" && last === "`" && !value.includes("${")) {
    return value.slice(1, -1);
  }

  return null;
};

const createDetectedImport = (
  source: string,
  region: ScriptRegion,
  specifier: string,
  importKind: DetectedImport["importKind"],
  named: string[],
  start: number,
  end: number,
  quoteEndOffset: number,
): DetectedImport => {
  const absoluteStart = region.offset + start;
  const absoluteEnd = region.offset + end;
  const absoluteQuoteEndOffset = region.offset + quoteEndOffset;

  return {
    specifier,
    packageName: getPackageName(specifier),
    named: [...named].sort(),
    importKind,
    runtime: region.runtime,
    line: positionAt(source, absoluteStart).line,
    quoteEnd: positionAt(source, absoluteQuoteEndOffset),
    statementRange: rangeFromOffsets(source, absoluteStart, absoluteEnd),
  };
};

const runtimeEntries = (entries: StaticImportEntry[]): StaticImportEntry[] =>
  entries.filter((entry) => !entry.isType);

const importsFromStaticImport = (source: string, region: ScriptRegion, item: StaticImport): DetectedImport[] => {
  const specifier = item.moduleRequest.value;

  if (!isRuntimePackageSpecifier(specifier)) {
    return [];
  }

  const entries = runtimeEntries(item.entries);

  if (entries.length === 0 && item.entries.length === 0) {
    return [
      createDetectedImport(source, region, specifier, "namespace", [], item.start, item.end, item.moduleRequest.end),
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
    imports.push(createDetectedImport(source, region, specifier, "default", [], item.start, item.end, item.moduleRequest.end));
  }

  if (entries.some((entry) => entry.importName.kind === ImportNameKind.NamespaceObject)) {
    imports.push(createDetectedImport(source, region, specifier, "namespace", [], item.start, item.end, item.moduleRequest.end));
  }

  if (named.length > 0) {
    imports.push(createDetectedImport(source, region, specifier, "named", named, item.start, item.end, item.moduleRequest.end));
  }

  return imports;
};

const importsFromStaticExport = (source: string, region: ScriptRegion, item: StaticExport): DetectedImport[] => {
  if (item.entries.length === 0) {
    return [];
  }

  const specifier = item.entries[0]?.moduleRequest?.value;
  if (!specifier || !isRuntimePackageSpecifier(specifier)) {
    return [];
  }

  const imports: DetectedImport[] = [];
  const named = item.entries
    .filter((entry) => entry.importName.kind === ExportImportNameKind.Name && entry.importName.name)
    .map((entry) => entry.importName.name as string);

  if (item.entries.some((entry) => entry.importName.kind === ExportImportNameKind.All || entry.importName.kind === ExportImportNameKind.AllButDefault)) {
    imports.push(createDetectedImport(source, region, specifier, "namespace", [], item.start, item.end, item.entries[0].moduleRequest!.end));
  }

  if (named.length > 0) {
    imports.push(createDetectedImport(source, region, specifier, "named", named, item.start, item.end, item.entries[0].moduleRequest!.end));
  }

  return imports;
};

const importsFromRegion = (source: string, region: ScriptRegion): DetectedImport[] => {
  const parsed = parseSync(region.filename, region.source, {
    ...parserOptions,
    lang: region.language,
  });
  const imports: DetectedImport[] = [];

  for (const item of parsed.module.staticImports) {
    imports.push(...importsFromStaticImport(source, region, item));
  }

  for (const item of parsed.module.staticExports) {
    imports.push(...importsFromStaticExport(source, region, item));
  }

  for (const item of parsed.module.dynamicImports) {
    const specifier = literalDynamicImportSpecifier(
      region.source.slice(item.moduleRequest.start, item.moduleRequest.end),
    );

    if (specifier && isRuntimePackageSpecifier(specifier)) {
      imports.push(createDetectedImport(source, region, specifier, "dynamic", [], item.start, item.end, item.moduleRequest.end));
    }
  }

  return imports;
};

export const extractRuntimeImports = (filename: string, source: string): DetectedImport[] => {
  const imports = scriptRegionsForDocument(filename, source).flatMap((region) => importsFromRegion(source, region));

  return imports.sort((left, right) => left.statementRange.start.line - right.statementRange.start.line);
};
