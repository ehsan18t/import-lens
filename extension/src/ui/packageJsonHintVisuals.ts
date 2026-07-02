export type PackageJsonPrimaryTone = "neutral" | "unavailable";

export type PackageJsonSuffixTone = "latest" | "update" | "install" | "stale";

export const primaryToneThemeColor = (tone: PackageJsonPrimaryTone): string => {
  if (tone === "unavailable") {
    return "list.errorForeground";
  }

  return "descriptionForeground";
};

export const suffixToneThemeColor = (tone: PackageJsonSuffixTone): string => {
  if (tone === "latest") {
    return "gitDecoration.addedResourceForeground";
  }

  if (tone === "stale") {
    return "problemsWarningIcon.foreground";
  }

  return "gitDecoration.modifiedResourceForeground";
};
