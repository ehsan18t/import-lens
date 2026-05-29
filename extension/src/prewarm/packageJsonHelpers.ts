import path from "node:path";

export interface PackageJsonPrewarmPayload {
  packageJsonPath: string;
  activeDocumentPath: string;
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
