import type { ImportKind } from "../ipc/protocol.js";

export type ImportRuntime = "component" | "server" | "client";
export type ImportSyntax = "static" | "reexport" | "star_reexport" | "dynamic";

export interface SourcePosition {
  line: number;
  character: number;
}

export interface SourceRange {
  start: SourcePosition;
  end: SourcePosition;
}

export interface DetectedImport {
  specifier: string;
  packageName: string;
  named: string[];
  importKind: ImportKind;
  syntax: ImportSyntax;
  runtime: ImportRuntime;
  line: number;
  quoteEnd: SourcePosition;
  statementRange: SourceRange;
}

export interface PackageResolutionFound {
  ok: true;
  packageName: string;
  packageRoot: string;
  packageJsonPath: string;
  version: string;
}

export interface PackageResolutionMissing {
  ok: false;
  packageName: string;
  reason: "package_not_found" | "invalid_package_json";
}

export type PackageResolution = PackageResolutionFound | PackageResolutionMissing;
