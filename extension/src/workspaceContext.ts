import { access } from "node:fs/promises";
import path from "node:path";

const exists = async (filePath: string): Promise<boolean> => {
  try {
    await access(filePath);
    return true;
  } catch {
    return false;
  }
};

export const analysisRootForFile = async (
  filePath: string,
  workspaceFolderPath?: string,
): Promise<string> => {
  if (workspaceFolderPath) {
    return workspaceFolderPath;
  }

  const fallback = path.dirname(filePath);
  let current = fallback;

  while (true) {
    if (
      (await exists(path.join(current, "package.json"))) ||
      (await exists(path.join(current, "node_modules")))
    ) {
      return current;
    }

    const parent = path.dirname(current);
    if (parent === current) {
      return fallback;
    }

    current = parent;
  }
};
