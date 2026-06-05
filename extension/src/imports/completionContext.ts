import {
  ImportNameKind,
  parseSync,
  type ParserOptions,
  type StaticImport,
  type StaticImportEntry,
} from "oxc-parser";

export interface NamedImportCompletionContext {
  specifier: string;
  importedNames: string[];
}

const parserOptions: ParserOptions = {
  sourceType: "module",
  astType: "ts",
  lang: "tsx",
  range: false,
};

export const namedImportCompletionContext = (
  source: string,
  offset: number,
): NamedImportCompletionContext | null => {
  const parsed = parseSync("import-lens-completion.tsx", source, parserOptions);

  for (const item of parsed.module.staticImports) {
    const range = namedImportMemberRange(source, item);

    if (!range || offset < range.start || offset > range.end) {
      continue;
    }

    return {
      specifier: item.moduleRequest.value,
      importedNames: importedNamesFromEntries(item.entries),
    };
  }

  return null;
};

const namedImportMemberRange = (
  source: string,
  item: StaticImport,
): { start: number; end: number } | null => {
  const statement = source.slice(item.start, item.end);
  const openBrace = statement.indexOf("{");

  if (openBrace === -1) {
    return null;
  }

  const closeBrace = statement.indexOf("}", openBrace + 1);

  if (closeBrace === -1) {
    return null;
  }

  return {
    start: item.start + openBrace + 1,
    end: item.start + closeBrace,
  };
};

const importedNamesFromEntries = (entries: StaticImportEntry[]): string[] =>
  entries
    .filter((entry) => entry.importName.kind === ImportNameKind.Name && entry.importName.name)
    .map((entry) => entry.importName.name as string);
