import type { ConfidenceLevel } from "../ipc/protocol.js";

export type ConfidenceTone = ConfidenceLevel | "unknown";

export interface ConfidenceVisual {
  badge: string;
  label: string;
  cssClass: string;
  themeColor: string;
  cssVariable: string;
  fallbackColor: string;
  fontWeight: string;
}

const confidenceVisuals: Record<ConfidenceTone, ConfidenceVisual> = {
  high: {
    badge: "High",
    label: "High confidence",
    cssClass: "confidence-high",
    themeColor: "charts.green",
    cssVariable: "--vscode-charts-green",
    fallbackColor: "#2ea043",
    fontWeight: "500",
  },
  medium: {
    badge: "Medium",
    label: "Medium confidence",
    cssClass: "confidence-medium",
    themeColor: "charts.yellow",
    cssVariable: "--vscode-charts-yellow",
    fallbackColor: "#d29922",
    fontWeight: "600",
  },
  low: {
    badge: "Low",
    label: "Low confidence",
    cssClass: "confidence-low",
    themeColor: "charts.red",
    cssVariable: "--vscode-charts-red",
    fallbackColor: "#f85149",
    fontWeight: "700",
  },
  unknown: {
    badge: "Unknown",
    label: "Unknown confidence",
    cssClass: "confidence-unknown",
    themeColor: "descriptionForeground",
    cssVariable: "--vscode-descriptionForeground",
    fallbackColor: "#8b949e",
    fontWeight: "400",
  },
};

/**
 * The `unknown` visual is the fallback for a level this build has never heard of, not just a tone
 * callers may pass deliberately. An older extension meets a newer daemon routinely, and an
 * unguarded lookup returned `undefined` for a fourth `ConfidenceLevel` — the caller then read
 * `.badge` off it and the whole hover rendered nothing rather than degrading. The asset-kind lookup
 * beside this one already renders an unknown wire value under its own name.
 */
export const confidenceVisualFor = (confidence: ConfidenceTone): ConfidenceVisual =>
  confidenceVisuals[confidence] ?? confidenceVisuals.unknown;

export const confidenceCssColor = (confidence: ConfidenceTone): string => {
  const visual = confidenceVisualFor(confidence);
  return `var(${visual.cssVariable}, ${visual.fallbackColor})`;
};
