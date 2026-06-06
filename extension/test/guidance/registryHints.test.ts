import assert from "node:assert/strict";
import test from "node:test";
import {
  fetchRegistryHint,
  getCachedRegistryHint,
  registryHintForPackage,
} from "../../src/guidance/registryHints.js";

interface StoredState {
  [key: string]: unknown;
}

const context = (initial: StoredState = {}) => {
  const state = { ...initial };
  let getCount = 0;

  return {
    context: {
      globalState: {
        get: <T>(key: string, fallback: T): T => {
          getCount++;
          return (key in state ? state[key] : fallback) as T;
        },
        update: async (key: string, value: unknown): Promise<void> => {
          state[key] = value;
        },
      },
    },
    getCount: () => getCount,
    state,
  };
};

const response = (
  status: number,
  body: unknown = {},
): Response => ({
  ok: status >= 200 && status < 300,
  status,
  headers: { get: () => null },
  json: async () => body,
} as unknown as Response);

test("fetchRegistryHint shares an in-flight request between concurrent callers", async () => {
  const store = context();
  let resolveFetch!: (value: Response) => void;
  const calls: string[] = [];
  const fetchImpl = (async (url: string | URL | Request) => {
    calls.push(String(url));
    return new Promise<Response>((resolve) => {
      resolveFetch = resolve;
    });
  }) as typeof fetch;

  const first = fetchRegistryHint(store.context as never, "shared-pkg", { fetchImpl });
  const second = fetchRegistryHint(store.context as never, "shared-pkg", { fetchImpl });

  await Promise.resolve();
  await Promise.resolve();
  resolveFetch(response(200, {
    "dist-tags": { latest: "2.0.0" },
    versions: { "2.0.0": {} },
  }));

  assert.deepEqual(await Promise.all([first, second]), [
    { latestVersion: "2.0.0", deprecated: false },
    { latestVersion: "2.0.0", deprecated: false },
  ]);
  assert.equal(calls.length, 1);
});

test("registryHintForPackage caches not-found responses without immediate refetch", async () => {
  const store = context();
  let calls = 0;
  const fetchImpl = (async () => {
    calls++;
    return response(404);
  }) as typeof fetch;

  assert.equal(await registryHintForPackage(store.context as never, "missing-pkg", { fetchImpl }), null);
  assert.equal(await registryHintForPackage(store.context as never, "missing-pkg", { fetchImpl }), null);
  assert.equal(calls, 1);
});

test("fetchRegistryHint retries transient failures with bounded delay", async () => {
  const store = context();
  const delays: number[] = [];
  let calls = 0;
  const fetchImpl = (async () => {
    calls++;
    return calls === 1
      ? response(503)
      : response(200, {
        "dist-tags": { latest: "1.0.0" },
        versions: { "1.0.0": {} },
      });
  }) as typeof fetch;

  const hint = await fetchRegistryHint(store.context as never, "flaky-pkg", {
    fetchImpl,
    sleep: async (delayMs: number) => {
      delays.push(delayMs);
    },
  });

  assert.deepEqual(hint, { latestVersion: "1.0.0", deprecated: false });
  assert.deepEqual(delays, [500]);
  assert.equal(calls, 2);
});

test("fetchRegistryHint checks deprecation for the installed version", async () => {
  const store = context();
  const fetchImpl = (async () => response(200, {
    "dist-tags": { latest: "2.0.0" },
    versions: {
      "1.0.0": { deprecated: "Use 2.x" },
      "2.0.0": {},
    },
  })) as typeof fetch;

  assert.deepEqual(
    await fetchRegistryHint(store.context as never, "deprecated-installed", {
      fetchImpl,
      installedVersion: "1.0.0",
    }),
    { latestVersion: "2.0.0", deprecated: true },
  );
});

test("getCachedRegistryHint hydrates global state once into memory", async () => {
  const cacheKey = "importLens.registryHints";
  const packageKey = "cached-pkg\n1.0.0";
  const store = context({
    [cacheKey]: {
      [packageKey]: {
        status: "ok",
        timestamp: Date.now(),
        latestVersion: "1.0.0",
        deprecated: false,
      },
    },
  });

  assert.deepEqual(
    getCachedRegistryHint(store.context as never, "cached-pkg", "1.0.0"),
    { latestVersion: "1.0.0", deprecated: false },
  );
  assert.deepEqual(
    getCachedRegistryHint(store.context as never, "cached-pkg", "1.0.0"),
    { latestVersion: "1.0.0", deprecated: false },
  );
  assert.equal(store.getCount(), 1);
});

test("getCachedRegistryHint treats legacy cache entries as positive hits", () => {
  const cacheKey = "importLens.registryHints";
  const packageKey = "legacy-pkg\n";
  const store = context({
    [cacheKey]: {
      [packageKey]: {
        timestamp: Date.now(),
        latestVersion: "1.0.0",
        deprecated: true,
      },
    },
  });

  assert.deepEqual(
    getCachedRegistryHint(store.context as never, "legacy-pkg"),
    { latestVersion: "1.0.0", deprecated: true },
  );
});
