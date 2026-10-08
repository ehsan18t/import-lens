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
  prewarmPackageJson(
    packageJsonPath: string,
    activeDocumentPath: string,
    workspaceRoot: string,
  ): void;
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

/**
 * Sends a prewarm for every file-backed manifest among `documents`, under the root package.json
 * analysis of the same document uses: the cache is sharded by workspace root, so a prewarm under
 * any other root fills a shard interactive analysis never reads.
 */
export const prewarmPackageJsonDocuments = async <TDocument extends PackageJsonPrewarmDocument>(
  documents: Iterable<TDocument>,
  target: PackageJsonPrewarmTarget,
  analysisRoot: (document: TDocument) => Promise<string>,
): Promise<number> => {
  const sends: Promise<void>[] = [];

  for (const document of documents) {
    if (document.uri.scheme !== "file") {
      continue;
    }

    const payload = packageJsonPrewarmPayload(document.uri.fsPath);

    if (!payload) {
      continue;
    }

    sends.push(
      analysisRoot(document).then((workspaceRoot) => {
        target.prewarmPackageJson(
          payload.packageJsonPath,
          payload.activeDocumentPath,
          workspaceRoot,
        );
      }),
    );
  }

  await Promise.all(sends);
  return sends.length;
};
