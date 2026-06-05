import type * as vscode from "vscode";

export const supportedLanguageIds: ReadonlySet<string> = new Set([
  "javascript",
  "typescript",
  "typescriptreact",
  "javascriptreact",
  "svelte",
  "astro",
]);

export const languageSelector: vscode.DocumentSelector = [
  { language: "javascript" },
  { language: "typescript" },
  { language: "typescriptreact" },
  { language: "javascriptreact" },
  { language: "svelte" },
  { language: "astro" },
];
