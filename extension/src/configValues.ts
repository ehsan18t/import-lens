/**
 * A megabyte budget as the daemon reads it: a whole number (`u64` on the wire). A fractional or
 * negative number fails the decode of the whole hello frame, which the daemon skips; every request
 * after it is then answered "hello message not received" while the extension believes the daemon
 * is ready. VS Code only warns about a value outside the schema, so it is normalized here: rounded,
 * raised to the schema's minimum, and replaced by the default when it is not a finite number.
 */
export const wholeMegabytes = (value: unknown, minimum: number, fallback: number): number =>
  typeof value === "number" && Number.isFinite(value)
    ? Math.max(minimum, Math.round(value))
    : fallback;
