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
  const fetchedAt = 1_000;
  let resolveFetch!: (value: Response) => void;
  const calls: string[] = [];
  const fetchImpl = (async (url: string | URL | Request) => {
    calls.push(String(url));
    return new Promise<Response>((resolve) => {
      resolveFetch = resolve;
    });
  }) as typeof fetch;

  const first = fetchRegistryHint(store.context as never, "shared-pkg", { fetchImpl, now: () => fetchedAt });
  const second = fetchRegistryHint(store.context as never, "shared-pkg", { fetchImpl, now: () => fetchedAt });

  await Promise.resolve();
  await Promise.resolve();
  resolveFetch(response(200, {
    "dist-tags": { latest: "2.0.0" },
    versions: { "2.0.0": {} },
  }));

  assert.deepEqual(await Promise.all([first, second]), [
    { latestVersion: "2.0.0", deprecated: false, fetchedAt },
    { latestVersion: "2.0.0", deprecated: false, fetchedAt },
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
  const fetchedAt = 2_000;
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
    now: () => fetchedAt,
    sleep: async (delayMs: number) => {
      delays.push(delayMs);
    },
  });

  assert.deepEqual(hint, { latestVersion: "1.0.0", deprecated: false, fetchedAt });
  assert.deepEqual(delays, [500]);
  assert.equal(calls, 2);
});

test("fetchRegistryHint checks deprecation for the installed version", async () => {
  const store = context();
  const fetchedAt = 3_000;
  const fetchImpl = (async () => response(200, {
    "dist-tags": { latest: "2.0.0" },
    time: { "2.0.0": "2026-06-05T10:00:00.000Z" },
    versions: {
      "1.0.0": { deprecated: "Use 2.x" },
      "2.0.0": {},
    },
  })) as typeof fetch;

  assert.deepEqual(
    await fetchRegistryHint(store.context as never, "deprecated-installed", {
      fetchImpl,
      installedVersion: "1.0.0",
      now: () => fetchedAt,
    }),
    {
      latestVersion: "2.0.0",
      latestPublishedAt: "2026-06-05T10:00:00.000Z",
      isLatest: false,
      deprecated: true,
      fetchedAt,
    },
  );
});

test("fetchRegistryHint marks installed versions that match latest", async () => {
  const store = context();
  const fetchedAt = 4_000;
  const fetchImpl = (async () => response(200, {
    "dist-tags": { latest: "2.0.0" },
    time: { "2.0.0": "2026-06-05T10:00:00.000Z" },
    versions: { "2.0.0": {} },
  })) as typeof fetch;

  assert.deepEqual(
    await fetchRegistryHint(store.context as never, "latest-installed", {
      fetchImpl,
      installedVersion: "2.0.0",
      now: () => fetchedAt,
    }),
    {
      latestVersion: "2.0.0",
      latestPublishedAt: "2026-06-05T10:00:00.000Z",
      isLatest: true,
      deprecated: false,
      fetchedAt,
    },
  );
});

test("registryHintForPackage refreshes successful cache entries after six hours", async () => {
  const cacheKey = "importLens.registryHints";
  const packageKey = "stale-pkg\n1.0.0";
  const store = context({
    [cacheKey]: {
      [packageKey]: {
        status: "ok",
        timestamp: 1_000,
        latestVersion: "1.0.0",
        isLatest: true,
        deprecated: false,
      },
    },
  });
  let calls = 0;
  const fetchImpl = (async () => {
    calls++;
    return response(200, {
      "dist-tags": { latest: "1.0.1" },
      versions: { "1.0.1": {} },
    });
  }) as typeof fetch;

  assert.deepEqual(
    await registryHintForPackage(store.context as never, "stale-pkg", {
      fetchImpl,
      installedVersion: "1.0.0",
      now: () => 1_000 + (6 * 60 * 60 * 1000) - 1,
    }),
    { latestVersion: "1.0.0", isLatest: true, deprecated: false, fetchedAt: 1_000 },
  );
  assert.equal(calls, 0);

  assert.deepEqual(
    await registryHintForPackage(store.context as never, "stale-pkg", {
      fetchImpl,
      installedVersion: "1.0.0",
      now: () => 1_000 + (6 * 60 * 60 * 1000),
    }),
    { latestVersion: "1.0.1", isLatest: false, deprecated: false, fetchedAt: 1_000 + (6 * 60 * 60 * 1000) },
  );
  assert.equal(calls, 1);
});

test("registryHintForPackage fetches install metadata without an installed version", async () => {
  const store = context();
  const fetchedAt = 5_000;
  const fetchImpl = (async () => response(200, {
    "dist-tags": { latest: "4.5.6" },
    time: { "4.5.6": "2026-06-05T10:00:00.000Z" },
    versions: { "4.5.6": {} },
  })) as typeof fetch;

  assert.deepEqual(
    await registryHintForPackage(store.context as never, "installable-pkg", {
      fetchImpl,
      now: () => fetchedAt,
    }),
    {
      latestVersion: "4.5.6",
      latestPublishedAt: "2026-06-05T10:00:00.000Z",
      deprecated: false,
      fetchedAt,
    },
  );
});

test("fetchRegistryHint force refresh bypasses a fresh cache entry", async () => {
  const cacheKey = "importLens.registryHints";
  const packageKey = "refresh-pkg\n1.0.0";
  const store = context({
    [cacheKey]: {
      [packageKey]: {
        status: "ok",
        timestamp: 1_000,
        latestVersion: "1.0.0",
        isLatest: true,
        deprecated: false,
      },
    },
  });
  let calls = 0;
  const fetchImpl = (async () => {
    calls++;
    return response(200, {
      "dist-tags": { latest: "2.0.0" },
      versions: { "2.0.0": {} },
    });
  }) as typeof fetch;

  assert.deepEqual(
    await fetchRegistryHint(store.context as never, "refresh-pkg", {
      fetchImpl,
      installedVersion: "1.0.0",
      forceRefresh: true,
      now: () => 2_000,
    }),
    { latestVersion: "2.0.0", isLatest: false, deprecated: false, fetchedAt: 2_000 },
  );
  assert.equal(calls, 1);
  assert.deepEqual(
    getCachedRegistryHint(store.context as never, "refresh-pkg", "1.0.0", { now: () => 2_001 }),
    { latestVersion: "2.0.0", isLatest: false, deprecated: false, fetchedAt: 2_000 },
  );
});

test("fetchRegistryHint force refresh shares concurrent in-flight requests", async () => {
  const store = context();
  let resolveFetch!: (value: Response) => void;
  const calls: string[] = [];
  const fetchImpl = (async (url: string | URL | Request) => {
    calls.push(String(url));
    return new Promise<Response>((resolve) => {
      resolveFetch = resolve;
    });
  }) as typeof fetch;

  const first = fetchRegistryHint(store.context as never, "force-shared-pkg", {
    fetchImpl,
    forceRefresh: true,
    now: () => 7_000,
  });
  const second = fetchRegistryHint(store.context as never, "force-shared-pkg", {
    fetchImpl,
    forceRefresh: true,
    now: () => 7_000,
  });

  await Promise.resolve();
  await Promise.resolve();
  resolveFetch(response(200, {
    "dist-tags": { latest: "3.0.0" },
    versions: { "3.0.0": {} },
  }));

  assert.deepEqual(await Promise.all([first, second]), [
    { latestVersion: "3.0.0", deprecated: false, fetchedAt: 7_000 },
    { latestVersion: "3.0.0", deprecated: false, fetchedAt: 7_000 },
  ]);
  assert.equal(calls.length, 1);
});

test("getCachedRegistryHint hydrates global state once into memory", async () => {
  const cacheKey = "importLens.registryHints";
  const packageKey = "cached-pkg\n1.0.0";
  const fetchedAt = 8_000;
  const store = context({
    [cacheKey]: {
      [packageKey]: {
        status: "ok",
        timestamp: fetchedAt,
        latestVersion: "1.0.0",
        deprecated: false,
      },
    },
  });

  assert.deepEqual(
    getCachedRegistryHint(store.context as never, "cached-pkg", "1.0.0", { now: () => fetchedAt + 1 }),
    { latestVersion: "1.0.0", isLatest: true, deprecated: false, fetchedAt },
  );
  assert.deepEqual(
    getCachedRegistryHint(store.context as never, "cached-pkg", "1.0.0", { now: () => fetchedAt + 1 }),
    { latestVersion: "1.0.0", isLatest: true, deprecated: false, fetchedAt },
  );
  assert.equal(store.getCount(), 1);
});

test("getCachedRegistryHint treats legacy cache entries as positive hits", () => {
  const cacheKey = "importLens.registryHints";
  const packageKey = "legacy-pkg\n";
  const fetchedAt = 9_000;
  const store = context({
    [cacheKey]: {
      [packageKey]: {
        timestamp: fetchedAt,
        latestVersion: "1.0.0",
        deprecated: true,
      },
    },
  });

  assert.deepEqual(
    getCachedRegistryHint(store.context as never, "legacy-pkg", undefined, { now: () => fetchedAt + 1 }),
    { latestVersion: "1.0.0", deprecated: true, fetchedAt },
  );
});
