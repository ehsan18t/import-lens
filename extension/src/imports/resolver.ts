import { access, readFile } from "node:fs/promises";
import path from "node:path";
import { getPackageName } from "./specifier.js";
import type { PackageResolution } from "./types.js";

const fileExists = async (filePath: string): Promise<boolean> => {
  try {
    await access(filePath);
    return true;
  } catch {
    return false;
  }
};

const parentDirectoriesFrom = (startPath: string): string[] => {
  const directories: string[] = [];
  let current = path.dirname(startPath);

  while (true) {
    directories.push(current);
    const parent = path.dirname(current);

    if (parent === current) {
      return directories;
    }

    current = parent;
  }
};

export const resolveInstalledPackage = async (specifier: string, activeDocumentPath: string): Promise<PackageResolution> => {
  const packageName = getPackageName(specifier);

  for (const directory of parentDirectoriesFrom(activeDocumentPath)) {
    const packageRoot = path.join(directory, "node_modules", packageName);
    const packageJsonPath = path.join(packageRoot, "package.json");

    if (!(await fileExists(packageJsonPath))) {
      continue;
    }

    try {
      const packageJson = JSON.parse(await readFile(packageJsonPath, "utf8")) as { version?: unknown };

      if (typeof packageJson.version !== "string") {
        return { ok: false, packageName, reason: "invalid_package_json" };
      }

      return {
        ok: true,
        packageName,
        packageRoot,
        packageJsonPath,
        version: packageJson.version,
      };
    } catch {
      return { ok: false, packageName, reason: "invalid_package_json" };
    }
  }

  return { ok: false, packageName, reason: "package_not_found" };
};

