import assert from "node:assert/strict";
import test from "node:test";
import {
  fetchRegistryHint,
  getCachedRegistryHint,
  registryQueueDefaults,
  resetRegistryHintStateForTests,
  type RegistryHintStore,
} from "../../src/guidance/registryHints.js";

class MemoryStore implements RegistryHintStore {
  readonly #values = new Map<string, unknown>();

  get<T>(key: string, defaultValue: T): T {
    return (this.#values.get(key) as T | undefined) ?? defaultValue;
  }

  update(key: string, value: unknown): void {
    this.#values.set(key, value);
  }
}

const packageMetadata = (latestVersion: string): unknown => ({
  "dist-tags": { latest: latestVersion },
  versions: {
    [latestVersion]: {},
  },
  time: {
    [latestVersion]: "2026-06-25T00:00:00.000Z",
  },
});

const jsonResponse = (body: unknown, status = 200, headers: HeadersInit = {}): Response =>
  new Response(JSON.stringify(body), { status, headers });

test("registry queue defaults stay fast but npm-friendly", () => {
  assert.deepEqual(registryQueueDefaults, {
    concurrency: 8,
    intervalCap: 20,
    interval: 1000,
  });
});

test("getCachedRegistryHint returns stale cache immediately when requested", () => {
  const store = new MemoryStore();

  store.update("importLens.registryHints", {
    "react\n18.2.0": {
      hint: {
        latestVersion: "19.0.0",
        isLatest: false,
        fetchedAt: 10,
      },
      updatedAt: 10,
    },
  });

  const hint = getCachedRegistryHint(store, "react", "18.2.0", {
    allowStale: true,
    now: 10_000_000,
  });

  assert.equal(hint?.latestVersion, "19.0.0");
});

test("fetchRegistryHint does not fetch when cached registry data is still fresh", async () => {
  resetRegistryHintStateForTests();
  const store = new MemoryStore();

  store.update("importLens.registryHints", {
    "react\n18.2.0": {
      hint: {
        latestVersion: "19.0.0",
        isLatest: false,
        fetchedAt: 100,
      },
      updatedAt: 100,
    },
  });

  const fetchImpl: typeof fetch = async () => {
    throw new Error("fresh cache should not hit the npm registry");
  };

  const hint = await fetchRegistryHint(store, "react", {
    installedVersion: "18.2.0",
    fetchImpl,
    now: () => 100 + 60_000,
  });

  assert.equal(hint?.latestVersion, "19.0.0");
});

test("fetchRegistryHint dedupes in-flight duplicate package/version targets", async () => {
  resetRegistryHintStateForTests();
  const store = new MemoryStore();
  let fetches = 0;
  const fetchImpl: typeof fetch = async () => {
    fetches += 1;
    await new Promise((resolve) => setTimeout(resolve, 5));
    return jsonResponse(packageMetadata("19.0.0"));
  };

  const [left, right] = await Promise.all([
    fetchRegistryHint(store, "react", {
      installedVersion: "18.2.0",
      fetchImpl,
      now: () => 100,
    }),
    fetchRegistryHint(store, "react", {
      installedVersion: "18.2.0",
      fetchImpl,
      now: () => 100,
    }),
  ]);

  assert.equal(fetches, 1);
  assert.equal(left?.latestVersion, "19.0.0");
  assert.deepEqual(right, left);
});

test("fetchRegistryHint honors Retry-After before retrying 429 responses", async () => {
  resetRegistryHintStateForTests();
  const store = new MemoryStore();
  const sleeps: number[] = [];
  let fetches = 0;
  const fetchImpl: typeof fetch = async () => {
    fetches += 1;

    if (fetches === 1) {
      return jsonResponse({ error: "rate limited" }, 429, { "Retry-After": "1" });
    }

    return jsonResponse(packageMetadata("19.0.0"));
  };

  const hint = await fetchRegistryHint(store, "react", {
    installedVersion: "18.2.0",
    fetchImpl,
    sleep: async (ms) => {
      sleeps.push(ms);
    },
    now: () => 100,
  });

  assert.equal(fetches, 2);
  assert.deepEqual(sleeps, [1000]);
  assert.equal(hint?.latestVersion, "19.0.0");
});
