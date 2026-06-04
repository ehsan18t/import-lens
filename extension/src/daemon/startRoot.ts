export const resolveDaemonStartRoot = (
  analysisRoot?: string,
  workspaceRoot?: string,
  previousAnalysisRoot?: string,
): string | undefined =>
  analysisRoot ?? workspaceRoot ?? previousAnalysisRoot;
