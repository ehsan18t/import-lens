import type * as vscode from "vscode";

export const supportedLanguageIds: ReadonlySet<string> = new Set([
  "javascript",
  "typescript",
  "typescriptreact",
  "javascriptreact",
  "svelte",
  "astro",
  "vue",
]);

export const languageSelector: vscode.DocumentSelector = [
  { language: "javascript", scheme: "file" },
  { language: "typescript", scheme: "file" },
  { language: "typescriptreact", scheme: "file" },
  { language: "javascriptreact", scheme: "file" },
  { language: "svelte", scheme: "file" },
  { language: "astro", scheme: "file" },
  { language: "vue", scheme: "file" },
];
