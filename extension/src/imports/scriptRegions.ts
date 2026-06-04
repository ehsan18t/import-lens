import type { ParserOptions } from "oxc-parser";
import type { ImportRuntime } from "./types.js";

export type ScriptLanguage = NonNullable<ParserOptions["lang"]>;

export type ScriptRuntime = ImportRuntime;

export interface ScriptRegion {
  filename: string;
  source: string;
  offset: number;
  language: ScriptLanguage;
  runtime: ScriptRuntime;
}

const svelteScriptPattern = /<script\b([^>]*)>([\s\S]*?)<\/script>/giu;
const astroClientScriptPattern = /<script\b([^>]*)>([\s\S]*?)<\/script>/giu;
const astroFrontmatterPattern = /^---(?:\r\n|\n|\r)([\s\S]*?)(?:\r\n|\n|\r)---(?:\r\n|\n|\r|$)/u;

const languageFromFilename = (filename: string): ScriptLanguage => {
  const lowerFilename = filename.toLowerCase();

  if (lowerFilename.endsWith(".tsx")) {
    return "tsx";
  }

  if (
    lowerFilename.endsWith(".ts") ||
    lowerFilename.endsWith(".mts") ||
    lowerFilename.endsWith(".cts")
  ) {
    return "ts";
  }

  if (lowerFilename.endsWith(".jsx")) {
    return "jsx";
  }

  return "js";
};

const svelteLanguageFromAttributes = (attributes: string): ScriptLanguage => {
  if (/\blang\s*=\s*(?:"ts"|'ts'|ts)(?:\s|$)/iu.test(attributes)) {
    return "ts";
  }

  return "js";
};

const blockFilename = (filename: string, language: ScriptLanguage, index: number): string =>
  `${filename}.${index}.${language}`;

const isProcessedAstroScript = (attributes: string): boolean => {
  const normalized = attributes.trim();

  if (normalized === "") {
    return true;
  }

  return /^src\s*=\s*(?:"[^"]*"|'[^']*'|[^\s>]+)\s*$/iu.test(normalized);
};

const svelteRegions = (filename: string, source: string): ScriptRegion[] => {
  const regions: ScriptRegion[] = [];

  for (const match of source.matchAll(svelteScriptPattern)) {
    const fullMatch = match[0];
    const attributes = match[1] ?? "";
    const scriptSource = match[2] ?? "";
    const matchIndex = match.index ?? 0;
    const contentOffset = matchIndex + fullMatch.indexOf(">") + 1;
    const language = svelteLanguageFromAttributes(attributes);

    regions.push({
      filename: blockFilename(filename, language, regions.length),
      source: scriptSource,
      offset: contentOffset,
      language,
      runtime: "component",
    });
  }

  return regions;
};

const astroRegions = (filename: string, source: string): ScriptRegion[] => {
  const regions: ScriptRegion[] = [];
  const frontmatter = astroFrontmatterPattern.exec(source);

  if (frontmatter?.[1] !== undefined && frontmatter.index === 0) {
    const fullMatch = frontmatter[0];
    const scriptSource = frontmatter[1];
    const contentOffset = fullMatch.indexOf(scriptSource);

    regions.push({
      filename: blockFilename(filename, "ts", regions.length),
      source: scriptSource,
      offset: contentOffset,
      language: "ts",
      runtime: "server",
    });
  }

  for (const match of source.matchAll(astroClientScriptPattern)) {
    const fullMatch = match[0];
    const attributes = match[1] ?? "";
    const scriptSource = match[2] ?? "";
    const matchIndex = match.index ?? 0;

    if (!isProcessedAstroScript(attributes)) {
      continue;
    }

    const contentOffset = matchIndex + fullMatch.indexOf(">") + 1;

    regions.push({
      filename: blockFilename(filename, "ts", regions.length),
      source: scriptSource,
      offset: contentOffset,
      language: "ts",
      runtime: "client",
    });
  }

  return regions;
};

export const scriptRegionsForDocument = (filename: string, source: string): ScriptRegion[] => {
  const lowerFilename = filename.toLowerCase();

  if (lowerFilename.endsWith(".svelte")) {
    return svelteRegions(filename, source);
  }

  if (lowerFilename.endsWith(".astro")) {
    return astroRegions(filename, source);
  }

  return [{ filename, source, offset: 0, language: languageFromFilename(filename), runtime: "component" }];
};
