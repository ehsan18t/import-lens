import type {
  DetectedImport,
  ImportKind,
  ImportRuntime,
  RefreshedImportIdentity,
} from "../ipc/protocol.js";

/**
 * What makes one import ONE import.
 *
 * **The specifier is not it.** `import React, { useState } from "react"` is a single statement, a
 * single specifier — and **two imports**: a default and a named. The daemon builds, measures, caches
 * and shares them separately, so anything on this side that keys by specifier alone silently merges
 * two different results into one and then reports nonsense about both. The SWR merge learned this
 * first (two variants collapsed into a single row); the shared-dependency insight learned it the
 * expensive way, telling users their shared bytes were "outside the public top-module breakdown"
 * when the sharer was the sibling import on the same line.
 *
 * `runtime` is part of it because it is part of the import: an Astro document can import the same
 * package with the same kind and the same named exports from its frontmatter (Server) and from a
 * client `<script>`, and each runtime ships its own artifact (ADR-0005).
 */
export interface ImportIdentity {
  specifier: string;
  importKind: ImportKind;
  named: readonly string[];
  runtime: ImportRuntime;
}

export const importIdentityOf = (detected: DetectedImport): ImportIdentity => ({
  specifier: detected.specifier,
  importKind: detected.importKind,
  named: detected.named,
  runtime: detected.runtime,
});

/** The same identity as an SWR / streamed push carries it (`import_kind` on the wire). */
export const refreshedImportIdentityOf = (identity: RefreshedImportIdentity): ImportIdentity => ({
  specifier: identity.specifier,
  importKind: identity.import_kind,
  named: identity.named,
  runtime: identity.runtime,
});

/**
 * A stable, order-independent key for one import. NUL/SOH separators keep the field boundaries
 * unambiguous (neither a specifier nor an export name can contain them), and `named` is sorted so a
 * differing source order still yields the same key.
 */
export const importIdentityKey = (identity: ImportIdentity): string =>
  `${identity.specifier}\u0000${identity.importKind}\u0000${identity.runtime}\u0000${[...identity.named].sort().join("\u0001")}`;

/**
 * How to NAME that import to a user — because "react" names two of them.
 *
 * The runtime is deliberately absent: this label is only ever read beside imports from the same file
 * and the same runtime (sharing is real nowhere else, ADR-0005), so printing it would add a word
 * that never varies.
 */
export const importIdentityLabel = (identity: ImportIdentity): string => {
  if (identity.importKind === "named") {
    return identity.named.length > 0
      ? `${identity.specifier} { ${identity.named.join(", ")} }`
      : identity.specifier;
  }

  return `${identity.specifier} (${identity.importKind})`;
};
