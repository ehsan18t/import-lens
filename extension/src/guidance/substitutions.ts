export interface ImportSubstitutionSuggestion {
  packageName: string;
  reason: string;
}

// A Map, not an object literal: specifiers come from user source, and a plain-object lookup of
// `constructor` or `__proto__` returns `Object.prototype` members instead of nothing.
const substitutionMap: ReadonlyMap<string, ImportSubstitutionSuggestion[]> = new Map([
  [
    "moment",
    [
      {
        packageName: "dayjs",
        reason: "Similar date API with a smaller common runtime footprint.",
      },
      {
        packageName: "date-fns",
        reason: "Function-level imports can tree-shake well in ESM builds.",
      },
    ],
  ],
  [
    "lodash",
    [
      {
        packageName: "lodash-es",
        reason: "ESM build gives bundlers better tree-shaking opportunities.",
      },
    ],
  ],
  [
    "uuid/v4",
    [
      {
        packageName: "uuid",
        reason: "Modern uuid exports named helpers from the package root.",
      },
    ],
  ],
]);

export const substitutionSuggestionsFor = (
  importSpecifier: string,
  packageName: string = importSpecifier,
): ImportSubstitutionSuggestion[] =>
  substitutionMap.get(importSpecifier) ?? substitutionMap.get(packageName) ?? [];
