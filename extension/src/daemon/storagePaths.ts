import path from "node:path";

interface StorageUriLike {
  fsPath: string;
}

export interface DaemonStorageContext {
  storageUri?: StorageUriLike;
  globalStorageUri: StorageUriLike;
}

export interface DaemonStoragePaths {
  cacheBasePath: string;
  lifecycleStoragePath: string;
}

export const resolveDaemonStoragePaths = (context: DaemonStorageContext): DaemonStoragePaths => {
  const lifecycleStoragePath = context.globalStorageUri.fsPath;
  const cacheBasePath = context.storageUri
    ? path.join(context.storageUri.fsPath, "daemon-cache")
    : path.join(lifecycleStoragePath, "workspace-cache");

  return {
    cacheBasePath,
    lifecycleStoragePath,
  };
};
