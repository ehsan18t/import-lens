import { execFile } from "node:child_process";
import path from "node:path";
import { promisify } from "node:util";

const execFileAsync = promisify(execFile);

// Above this many cells the line LCS is skipped and the change is treated as
// "too large to diff cheaply" (no badges), keeping mid-keystroke work bounded.
const maxLcsCells = 4_000_000;

/**
 * Returns the 0-based line numbers in `current` that are inserted or replaced
 * relative to `base`, computed with a line-level longest-common-subsequence.
 * Both inputs are split on LF/CRLF so line endings never register as changes.
 */
export const changedLinesBetween = (base: string, current: string): Set<number> => {
  const changed = new Set<number>();
  if (base === current) {
    return changed;
  }

  const before = base.split(/\r?\n/u);
  const after = current.split(/\r?\n/u);

  let start = 0;
  while (start < before.length && start < after.length && before[start] === after[start]) {
    start += 1;
  }

  let endBefore = before.length;
  let endAfter = after.length;
  while (
    endBefore > start
    && endAfter > start
    && before[endBefore - 1] === after[endAfter - 1]
  ) {
    endBefore -= 1;
    endAfter -= 1;
  }

  const n = endBefore - start;
  const m = endAfter - start;

  if (m === 0) {
    return changed; // pure deletion: no current line changed
  }
  if (n === 0) {
    for (let line = 0; line < m; line += 1) {
      changed.add(start + line);
    }
    return changed;
  }
  if (n * m > maxLcsCells) {
    return changed; // edit region too large to diff mid-keystroke; degrade to no badges
  }

  // dp[i][j] = LCS length of before[start+i..] and after[start+j..].
  const dp: Int32Array[] = Array.from({ length: n + 1 }, () => new Int32Array(m + 1));
  for (let i = n - 1; i >= 0; i -= 1) {
    for (let j = m - 1; j >= 0; j -= 1) {
      dp[i][j] = before[start + i] === after[start + j]
        ? dp[i + 1][j + 1] + 1
        : Math.max(dp[i + 1][j], dp[i][j + 1]);
    }
  }

  let i = 0;
  let j = 0;
  while (i < n && j < m) {
    if (before[start + i] === after[start + j]) {
      i += 1;
      j += 1;
    } else if (dp[i + 1][j] >= dp[i][j + 1]) {
      i += 1; // line deleted from base
    } else {
      changed.add(start + j); // line inserted into current
      j += 1;
    }
  }
  while (j < m) {
    changed.add(start + j);
    j += 1;
  }

  return changed;
};

/**
 * Lines changed in the working-tree buffer relative to its committed (HEAD)
 * content. Compares the in-memory `currentText` against the HEAD blob so deltas
 * stay accurate while the buffer is dirty. Returns an empty set for files not in
 * HEAD (untracked/new) or when git is unavailable, matching prior behavior.
 */
export const changedLinesForFile = async (
  fileName: string,
  currentText: string,
): Promise<Set<number>> => {
  if (!(await isGitRepository(fileName))) {
    return new Set();
  }

  try {
    const directory = path.dirname(fileName);
    const { stdout: topLevel } = await execFileAsync(
      "git",
      ["-C", directory, "rev-parse", "--show-toplevel"],
      { encoding: "utf8", timeout: 500 },
    );
    const relativePath = path
      .relative(topLevel.trim(), fileName)
      .split(path.sep)
      .join("/");
    const { stdout: baseText } = await execFileAsync(
      "git",
      ["-C", directory, "show", `HEAD:${relativePath}`],
      { encoding: "utf8", maxBuffer: 8 * 1024 * 1024, timeout: 1500 },
    );

    return changedLinesBetween(baseText, currentText);
  } catch {
    return new Set();
  }
};

const isGitRepository = async (fileName: string): Promise<boolean> => {
  try {
    const { stdout } = await execFileAsync(
      "git",
      ["-C", path.dirname(fileName), "rev-parse", "--is-inside-work-tree"],
      {
        encoding: "utf8",
        timeout: 500,
      },
    );

    return stdout.trim() === "true";
  } catch {
    return false;
  }
};
