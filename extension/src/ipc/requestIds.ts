export type RequestIdClock = () => number;

export const createIpcRequestIdGenerator = (clock: RequestIdClock = Date.now): (() => number) => {
  let current = Math.max(0, Math.trunc(clock()));

  return () => {
    current = Math.max(current + 1, Math.trunc(clock()));
    return current;
  };
};

export const nextIpcRequestId: () => number = createIpcRequestIdGenerator();
