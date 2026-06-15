export type InlineHintTone =
  | "size"
  | "sizeMedium"
  | "sizeLow"
  | "neutral"
  | "tag"
  | "info"
  | "action"
  | "delta"
  | "caution"
  | "alert";

export interface InlineHintVisual {
  readonly themeColorId: string;
  readonly fontStyle: string;
  readonly fontWeight: string;
}

const inlineHintVisuals: Record<InlineHintTone, InlineHintVisual> = {
  size: {
    themeColorId: "gitDecoration.addedResourceForeground",
    fontStyle: "italic",
    fontWeight: "400",
  },
  sizeMedium: {
    themeColorId: "gitDecoration.modifiedResourceForeground",
    fontStyle: "italic",
    fontWeight: "400",
  },
  sizeLow: {
    themeColorId: "gitDecoration.deletedResourceForeground",
    fontStyle: "italic",
    fontWeight: "400",
  },
  neutral: {
    themeColorId: "editorCodeLens.foreground",
    fontStyle: "italic",
    fontWeight: "400",
  },
  tag: {
    themeColorId: "descriptionForeground",
    fontStyle: "italic",
    fontWeight: "400",
  },
  info: {
    themeColorId: "gitDecoration.addedResourceForeground",
    fontStyle: "italic",
    fontWeight: "400",
  },
  action: {
    themeColorId: "gitDecoration.modifiedResourceForeground",
    fontStyle: "italic",
    fontWeight: "400",
  },
  delta: {
    themeColorId: "gitDecoration.modifiedResourceForeground",
    fontStyle: "italic",
    fontWeight: "400",
  },
  caution: {
    themeColorId: "gitDecoration.modifiedResourceForeground",
    fontStyle: "italic",
    fontWeight: "400",
  },
  alert: {
    themeColorId: "list.errorForeground",
    fontStyle: "italic",
    fontWeight: "400",
  },
};

export const inlineHintVisualFor = (tone: InlineHintTone): InlineHintVisual =>
  inlineHintVisuals[tone];

export const inlineHintThemeColorId = (tone: InlineHintTone): string =>
  inlineHintVisualFor(tone).themeColorId;
