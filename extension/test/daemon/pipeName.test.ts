import assert from "node:assert/strict";
import test from "node:test";
import { daemonPipeName } from "../../src/daemon/pipeName.js";

const macTempDirectory = "/var/folders/ql/0j2bfdv54q1dq2bcw7q2t2v40000gn/T";

test("daemonPipeName keeps a macOS socket in $TMPDIR within the sun_path limit", () => {
  const name = daemonPipeName("darwin", 4242, macTempDirectory);

  assert.ok(name.startsWith(`${macTempDirectory}/`), name);
  assert.ok(Buffer.byteLength(name) <= 103, `${Buffer.byteLength(name)} bytes: ${name}`);
});

test("daemonPipeName falls back to /tmp when the temp directory would overflow the limit", () => {
  const longTempDirectory = `/Users/someone/${"x".repeat(80)}`;

  for (const [platform, limit] of [
    ["darwin", 103],
    ["linux", 107],
  ] as const) {
    const name = daemonPipeName(platform, 4242, longTempDirectory);

    assert.ok(name.startsWith("/tmp/"), `${platform}: ${name}`);
    assert.ok(Buffer.byteLength(name) <= limit, `${platform}: ${name}`);
  }
});

test("daemonPipeName gives every spawn a distinct endpoint", () => {
  assert.notEqual(
    daemonPipeName("darwin", 4242, macTempDirectory),
    daemonPipeName("darwin", 4242, macTempDirectory),
  );
});

test("daemonPipeName uses a per-process named pipe on Windows", () => {
  assert.match(daemonPipeName("win32", 4242, "C:\\Temp"), /^\\\\\.\\pipe\\import-lens-4242-/u);
});
