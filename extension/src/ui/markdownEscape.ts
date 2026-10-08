// Hover markdown is trusted for a few commands, so any text that did not come from the extension
// itself (a package.json key, a registry field, a daemon message, an import specifier) must reach it
// through these helpers. Raw, `[x](command:...)` in a dependency key renders as a live link that
// runs an enabled command with attacker-chosen arguments.

const markdownMetacharacters = /[\\`*_{}[\]()#+\-.!|<>$~&]/gu;

/** Backslash-escapes every Markdown metacharacter, so the text renders literally. */
export const escapeMarkdown = (text: string): string =>
  text.replace(markdownMetacharacters, "\\$&");

/**
 * The query of a `command:` link carrying `args`. `encodeURIComponent` leaves `(` and `)` alone, and
 * an unbalanced `)` in an argument would end the link destination early.
 */
export const commandUriArgs = (args: readonly unknown[]): string =>
  encodeURIComponent(JSON.stringify(args)).replace(
    /[()]/gu,
    (paren) => `%${paren.charCodeAt(0).toString(16).toUpperCase()}`,
  );
