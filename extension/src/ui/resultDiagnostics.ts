import type { ImportResult } from "../ipc/protocol.js";

export const isTypesOnlyResult = (result: ImportResult): boolean =>
  result.diagnostics.some((diagnostic) => diagnostic.stage === "types_only");

/**
 * A native-binary-only package: it ships a platform-specific native binary and no importable JS
 * entry, so the daemon answers it MEASURED at zero and labels it. Rendered the same shape as
 * `types only` — a badge, not a byte size. The stage string is the daemon contract
 * (`pipeline::stage::NATIVE_BINARY_ONLY`).
 */
export const isNativeBinaryOnlyResult = (result: ImportResult): boolean =>
  result.diagnostics.some((diagnostic) => diagnostic.stage === "native_binary_only");

/**
 * A native-binary-backed package whose JS entry resolved and was measured: the size is real (the JS
 * shim), and this flag says the tool's work lives in a native binary. Rides a successful
 * measurement, so it sits BESIDE the size rather than replacing it. The stage string is the daemon
 * contract (`pipeline::stage::NATIVE_BINARY`).
 */
export const isNativeBinaryResult = (result: ImportResult): boolean =>
  result.diagnostics.some((diagnostic) => diagnostic.stage === "native_binary");
