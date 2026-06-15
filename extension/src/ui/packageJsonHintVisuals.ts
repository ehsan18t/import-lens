export type PackageJsonPrimaryTone = "neutral" | "unavailable";

export type PackageJsonSuffixTone = "latest" | "update" | "install";

export const primaryToneThemeColor = (tone: PackageJsonPrimaryTone): string => {
  if (tone === "unavailable") {
    return "charts.red";
  }

  return "descriptionForeground";
};

export const suffixToneThemeColor = (tone: PackageJsonSuffixTone): string => {
  if (tone === "latest") {
    return "charts.green";
  }

  return "charts.yellow";
};
