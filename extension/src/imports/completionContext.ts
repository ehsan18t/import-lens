export interface NamedImportCompletionContext {
  specifier: string;
  importedNames: string[];
}

const namedImportPattern =
  /\bimport\s*\{(?<members>[\s\S]*?)\}\s*from\s*(?<quote>["'])(?<specifier>[^"']+)\k<quote>/gu;

export const namedImportCompletionContext = (
  source: string,
  offset: number,
): NamedImportCompletionContext | null => {
  for (const match of source.matchAll(namedImportPattern)) {
    const matchStart = match.index ?? 0;
    const members = match.groups?.members ?? "";
    const membersStart = matchStart + match[0].indexOf("{") + 1;
    const membersEnd = membersStart + members.length;

    if (offset < membersStart || offset > membersEnd) {
      continue;
    }

    return {
      specifier: match.groups?.specifier ?? "",
      importedNames: importedNamesFromMembers(members),
    };
  }

  return null;
};

const importedNamesFromMembers = (members: string): string[] =>
  members
    .split(",")
    .map((member) => member.trim())
    .filter(Boolean)
    .map((member) => member.replace(/^type\s+/u, ""))
    .map((member) => member.split(/\s+as\s+/iu)[0]?.trim() ?? "")
    .filter(Boolean);
