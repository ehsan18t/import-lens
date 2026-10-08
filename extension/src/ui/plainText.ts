/**
 * The text of a hover's Markdown, for a surface that renders none (a message dialog's detail).
 *
 * Command links are dropped, text and all: a dialog cannot run them, and "Copy diagnostics" printed
 * as plain words offers an action that does nothing. Other links keep their text. Codicons
 * (`$(warning)`), emphasis and code markers are stripped, and backslash escapes are undone, so the
 * reader sees the words the hover shows.
 */
export const plainTextFromMarkdown = (markdown: string): string =>
  markdown
    .replace(/\[([^\]]*)\]\(command:[^)]*\)/gu, "")
    .replace(/\[([^\]]*)\]\([^)]*\)/gu, "$1")
    .replace(/\$\([a-z0-9-]+\)\s?/gu, "")
    .replace(/\*\*|__|`/gu, "")
    // CommonMark: a backslash escapes any ASCII punctuation character (`escapeMarkdown`'s output).
    .replace(/\\([!-/:-@[-`{-~])/gu, "$1")
    .replace(/^[ \t]*- \s*$/gmu, "")
    .replace(/\n{3,}/gu, "\n\n")
    .trim();
