import type { DetectedImport } from "../imports/types.js";
import type { ImportRequest } from "../ipc/protocol.js";

export const createImportRequest = (detected: DetectedImport, version: string): ImportRequest => ({
  specifier: detected.specifier,
  package: detected.packageName,
  version,
  named: detected.named,
  import_kind: detected.importKind,
  runtime: detected.runtime,
});
