import path from "node:path";

export interface PackageJsonPrewarmPayload {
  packageJsonPath: string;
  activeDocumentPath: string;
}

export interface PackageJsonPrewarmDocument {
  uri: {
    scheme: string;
    fsPath: string;
  };
}

export interface PackageJsonPrewarmTarget {
  prewarmPackageJson(packageJsonPath: string, activeDocumentPath: string): void;
}

export const isPackageJsonPath = (filePath: string): boolean =>
  path.basename(filePath) === "package.json";

export const packageJsonPrewarmPayload = (filePath: string): PackageJsonPrewarmPayload | null => {
  if (!isPackageJsonPath(filePath)) {
    return null;
  }

  return {
    packageJsonPath: filePath,
    activeDocumentPath: filePath,
  };
};

export const prewarmPackageJsonDocuments = (
  documents: Iterable<PackageJsonPrewarmDocument>,
  target: PackageJsonPrewarmTarget,
): number => {
  let sent = 0;

  for (const document of documents) {
    if (document.uri.scheme !== "file") {
      continue;
    }

    const payload = packageJsonPrewarmPayload(document.uri.fsPath);

    if (!payload) {
      continue;
    }

    target.prewarmPackageJson(payload.packageJsonPath, payload.activeDocumentPath);
    sent++;
  }

  return sent;
};
