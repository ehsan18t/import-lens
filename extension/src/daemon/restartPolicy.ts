const FIRST_RESTART_DELAY_MS = 1000;
const MAX_RESTART_DELAY_MS = 30000;
const CRASH_WINDOW_MS = 60000;
const MAX_CRASHES_IN_WINDOW = 3;

export const restartDelayMs = (attempt: number): number => {
  const exponent = Math.max(0, attempt - 1);
  return Math.min(FIRST_RESTART_DELAY_MS * 2 ** exponent, MAX_RESTART_DELAY_MS);
};

export const recentCrashTimes = (
  crashTimes: readonly number[],
  now: number = Date.now(),
): number[] => crashTimes.filter((crashTime) => now - crashTime <= CRASH_WINDOW_MS);

export const shouldEnterCrashDegradedMode = (
  crashTimes: readonly number[],
  now: number = Date.now(),
): boolean => recentCrashTimes(crashTimes, now).length >= MAX_CRASHES_IN_WINDOW;
