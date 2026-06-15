export type PackageJsonPrimaryTone = "neutral" | "unavailable";

export type PackageJsonSuffixTone = "latest" | "update" | "install";

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

  return "gitDecoration.modifiedResourceForeground";
};
