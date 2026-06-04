import { execFile } from "node:child_process";
import path from "node:path";
import { promisify } from "node:util";

const execFileAsync = promisify(execFile);
const hunkPattern = /^@@ -\d+(?:,\d+)? \+(\d+)(?:,(\d+))? @@/u;

export const changedLinesFromGitDiff = (diff: string): Set<number> => {
  const changedLines = new Set<number>();
  let newLine: number | null = null;

  for (const line of diff.split(/\r?\n/u)) {
    const hunk = hunkPattern.exec(line);

    if (hunk) {
      const start = Number(hunk[1]);
      const count = hunk[2] === undefined ? 1 : Number(hunk[2]);
      newLine = count === 0 ? null : start - 1;
      continue;
    }

    if (newLine === null) {
      continue;
    }

    if (line.startsWith("+++") || line.startsWith("---")) {
      continue;
    }

    if (line.startsWith("+")) {
      changedLines.add(newLine);
      newLine += 1;
      continue;
    }

    if (line.startsWith("-")) {
      continue;
    }

    if (line.startsWith(" ")) {
      newLine += 1;
    }
  }

  return changedLines;
};

export const changedLinesForFile = async (fileName: string): Promise<Set<number>> => {
  try {
    const { stdout } = await execFileAsync(
      "git",
      ["-C", path.dirname(fileName), "diff", "--no-ext-diff", "--unified=0", "HEAD", "--", fileName],
      {
        encoding: "utf8",
        maxBuffer: 1024 * 1024,
        timeout: 1500,
      },
    );

    return changedLinesFromGitDiff(stdout);
  } catch {
    return new Set();
  }
};
