import assert from "node:assert/strict";
import test from "node:test";
import type { PackageJsonDependencyHintState } from "../../src/guidance/packageJsonState.js";
import {
  RegistryHintRefresher,
  type RegistryRefreshHost,
  type RegistryRefreshTransport,
} from "../../src/guidance/registryRefresh.js";
import type {
  RefreshRegistryHintsRequest,
  RefreshRegistryHintsResponse,
  RegistryHintTarget,
} from "../../src/ipc/protocol.js";

const uriKey = "file:///workspace/package.json";

const silentLogger = {
  debug: (): void => undefined,
  warn: (): void => undefined,
};

const stateFor = (
  name: string,
  overrides: Partial<PackageJsonDependencyHintState> = {},
): PackageJsonDependencyHintState => ({
  name,
  section: "dependencies",
  status: "ready",
  installedVersion: "1.0.0",
  ...overrides,
});

const targetFor = (state: PackageJsonDependencyHintState): RegistryHintTarget => ({
  name: state.name,
  installedVersion: state.installedVersion,
});

interface TestHarness {
  host: RegistryRefreshHost<string, PackageJsonDependencyHintState>;
  current(name: string): PackageJsonDependencyHintState | undefined;
}

const createHarness = (initial: PackageJsonDependencyHintState[]): TestHarness => {
  const states = new Map<string, PackageJsonDependencyHintState[]>([[uriKey, initial]]);

  return {
    host: {
      keyFor: (uri) => uri,
      getStates: (uri) => states.get(uri),
      setStates: (uri, next) => {
        states.set(uri, next);
      },
    },
    current: (name) => states.get(uriKey)?.find((state) => state.name === name),
  };
};

const successResponse = (
  request: RefreshRegistryHintsRequest,
  fetchedAt: number,
): RefreshRegistryHintsResponse => ({
  version: request.version,
  request_id: request.request_id,
  results: request.targets.map((target) => ({
    target,
    hint: { latestVersion: "19.0.0", isLatest: false, fetchedAt },
    error: null,
  })),
  error: null,
  diagnostics: [],
});

test("daemon unavailable marks pending targets with a refresh error and stale only where cached", async () => {
  const react = stateFor("react", {
    registryHint: { latestVersion: "18.9.0", isLatest: false, fetchedAt: 50 },
  });
  const vue = stateFor("vue");
  const harness = createHarness([react, vue]);
  const daemon: RegistryRefreshTransport = {
    refreshRegistryHints: () => Promise.resolve(null),
  };
  const refresher = new RegistryHintRefresher(daemon, harness.host, silentLogger);

  await refresher.refresh(uriKey, [targetFor(react), targetFor(vue)], "refresh_stale");

  assert.equal(harness.current("react")?.registryHintRefreshStatus, "stale");
  assert.equal(harness.current("react")?.registryHintRefreshError, "Daemon unavailable");
  assert.equal(harness.current("react")?.registryHint?.latestVersion, "18.9.0");
  assert.equal(harness.current("vue")?.registryHintRefreshStatus, undefined);
  assert.equal(harness.current("vue")?.registryHintRefreshError, "Daemon unavailable");
  assert.equal(harness.current("vue")?.registryHint, undefined);
});

test("final response error only fails targets not completed by earlier partials", async () => {
  const react = stateFor("react");
  const vue = stateFor("vue", {
    registryHint: { latestVersion: "3.4.0", isLatest: false, fetchedAt: 50 },
  });
  const harness = createHarness([react, vue]);
  const daemon: RegistryRefreshTransport = {
    refreshRegistryHints: (request, onPartial) => {
      const reactResult = {
        target: request.targets[0],
        hint: { latestVersion: "19.0.0", isLatest: false, fetchedAt: 200 },
        error: null,
      };
      onPartial?.({
        version: request.version,
        request_id: request.request_id,
        results: [reactResult],
        indexes: [0],
        error: null,
        diagnostics: [],
      });

      return Promise.resolve({
        version: request.version,
        request_id: request.request_id,
        results: [reactResult],
        error: "registry offline",
        diagnostics: [],
      });
    },
  };
  const refresher = new RegistryHintRefresher(daemon, harness.host, silentLogger);

  await refresher.refresh(uriKey, [targetFor(react), targetFor(vue)], "force_refresh");

  assert.equal(harness.current("react")?.registryHintRefreshStatus, "fresh");
  assert.equal(harness.current("react")?.registryHintRefreshError, null);
  assert.equal(harness.current("react")?.registryHint?.latestVersion, "19.0.0");
  assert.equal(harness.current("vue")?.registryHintRefreshStatus, "stale");
  assert.equal(harness.current("vue")?.registryHintRefreshError, "registry offline");
  assert.equal(harness.current("vue")?.registryHint?.latestVersion, "3.4.0");
});

test("straggler from before forget cannot clobber status of refreshes started after forget", async () => {
  const react = stateFor("react", {
    registryHint: { latestVersion: "18.9.0", isLatest: false, fetchedAt: 50 },
  });
  const harness = createHarness([react]);
  let releaseStraggler!: (response: RefreshRegistryHintsResponse | null) => void;
  const stragglerResult = new Promise<RefreshRegistryHintsResponse | null>((resolve) => {
    releaseStraggler = resolve;
  });
  let calls = 0;
  const daemon: RegistryRefreshTransport = {
    refreshRegistryHints: (request) => {
      calls++;

      if (calls === 1) {
        return stragglerResult;
      }

      return Promise.resolve(successResponse(request, 200));
    },
  };
  const refresher = new RegistryHintRefresher(daemon, harness.host, silentLogger);

  const straggler = refresher.refresh(uriKey, [targetFor(react)], "refresh_stale");
  refresher.forget(uriKey);
  await refresher.refresh(uriKey, [targetFor(react)], "refresh_stale");

  assert.equal(harness.current("react")?.registryHintRefreshStatus, "fresh");

  releaseStraggler(null);
  await straggler;

  assert.equal(harness.current("react")?.registryHintRefreshStatus, "fresh");
  assert.equal(harness.current("react")?.registryHintRefreshError, null);
});

test("late failure from a superseded refresh does not downgrade fresh status", async () => {
  const react = stateFor("react", {
    registryHint: { latestVersion: "18.9.0", isLatest: false, fetchedAt: 50 },
  });
  const harness = createHarness([react]);
  let releaseFirst!: (response: RefreshRegistryHintsResponse | null) => void;
  const firstResult = new Promise<RefreshRegistryHintsResponse | null>((resolve) => {
    releaseFirst = resolve;
  });
  let calls = 0;
  const daemon: RegistryRefreshTransport = {
    refreshRegistryHints: (request) => {
      calls++;

      if (calls === 1) {
        return firstResult;
      }

      return Promise.resolve(successResponse(request, 200));
    },
  };
  const refresher = new RegistryHintRefresher(daemon, harness.host, silentLogger);

  const first = refresher.refresh(uriKey, [targetFor(react)], "refresh_stale");
  const second = refresher.refresh(uriKey, [targetFor(react)], "force_refresh");

  await second;
  assert.equal(harness.current("react")?.registryHintRefreshStatus, "fresh");
  assert.equal(harness.current("react")?.registryHintRefreshError, null);

  releaseFirst(null);
  await first;

  assert.equal(harness.current("react")?.registryHintRefreshStatus, "fresh");
  assert.equal(harness.current("react")?.registryHintRefreshError, null);
  assert.equal(harness.current("react")?.registryHint?.fetchedAt, 200);
});

test("a later refresh of a disjoint target does not supersede an earlier target's response", async () => {
  const react = stateFor("react");
  const vue = stateFor("vue");
  const harness = createHarness([react, vue]);

  let reactRequest: RefreshRegistryHintsRequest | undefined;
  let resolveReact: ((response: RefreshRegistryHintsResponse | null) => void) | undefined;
  const daemon: RegistryRefreshTransport = {
    refreshRegistryHints: (request) => {
      if (request.targets.some((target) => target.name === "react")) {
        reactRequest = request;
        return new Promise((resolve) => {
          resolveReact = resolve;
        });
      }
      return Promise.resolve(successResponse(request, 200));
    },
  };
  const refresher = new RegistryHintRefresher(daemon, harness.host, silentLogger);

  // The react refresh stays pending; the vue refresh (a disjoint target)
  // completes and bumps the generation.
  const reactRefresh = refresher.refresh(uriKey, [targetFor(react)], "refresh_stale");
  await refresher.refresh(uriKey, [targetFor(vue)], "refresh_stale");

  // Completing react's response must still apply react's fresh status, because
  // the vue refresh never touched the react target.
  assert.ok(reactRequest, "react refresh should have called the daemon");
  resolveReact?.(successResponse(reactRequest, 300));
  await reactRefresh;

  assert.equal(harness.current("react")?.registryHintRefreshStatus, "fresh");
});

test("logs a cache/network/failed summary from result origins", async () => {
  const a = stateFor("a");
  const b = stateFor("b");
  const c = stateFor("c");
  const harness = createHarness([a, b, c]);
  const messages: string[] = [];
  const logger = {
    debug: (message: string): void => void messages.push(message),
    warn: (): void => undefined,
  };
  const daemon: RegistryRefreshTransport = {
    refreshRegistryHints: (request) =>
      Promise.resolve({
        version: request.version,
        request_id: request.request_id,
        results: [
          {
            target: request.targets[0],
            hint: { latestVersion: "1", isLatest: true, fetchedAt: 1 },
            error: null,
            origin: "cache",
          },
          {
            target: request.targets[1],
            hint: { latestVersion: "2", isLatest: false, fetchedAt: 1 },
            error: null,
            origin: "network",
          },
          { target: request.targets[2], hint: null, error: "boom", origin: "network" },
        ],
        error: null,
        diagnostics: [],
      }),
  };
  const refresher = new RegistryHintRefresher(daemon, harness.host, logger);

  await refresher.refresh(uriKey, [targetFor(a), targetFor(b), targetFor(c)], "refresh_stale");

  assert.ok(
    messages.some((message) => message.includes("3 target(s): 1 cached, 1 fetched, 1 failed")),
    `expected a summary line, got: ${messages.join(" | ")}`,
  );
});

test("verbose mode logs per-package cache/network lines", async () => {
  const a = stateFor("a");
  const harness = createHarness([a]);
  const messages: string[] = [];
  const logger = {
    debug: (message: string): void => void messages.push(message),
    warn: (): void => undefined,
  };
  const daemon: RegistryRefreshTransport = {
    refreshRegistryHints: (request) =>
      Promise.resolve({
        version: request.version,
        request_id: request.request_id,
        results: [
          {
            target: request.targets[0],
            hint: { latestVersion: "1", isLatest: true, fetchedAt: 1 },
            error: null,
            origin: "network",
          },
        ],
        error: null,
        diagnostics: [],
      }),
  };
  const refresher = new RegistryHintRefresher(daemon, harness.host, logger, () => true);

  await refresher.refresh(uriKey, [targetFor(a)], "refresh_stale");

  assert.ok(
    messages.some((message) => message.includes("fetched (network) for a")),
    messages.join(" | "),
  );
});

test("logs the summary once even when the daemon streams partials", async () => {
  const a = stateFor("a");
  const b = stateFor("b");
  const harness = createHarness([a, b]);
  const messages: string[] = [];
  const logger = {
    debug: (message: string): void => void messages.push(message),
    warn: (): void => undefined,
  };
  const daemon: RegistryRefreshTransport = {
    refreshRegistryHints: (request, onPartial) => {
      const results = [
        {
          target: request.targets[0],
          hint: { latestVersion: "1", isLatest: true, fetchedAt: 1 },
          error: null,
          origin: "network" as const,
        },
        {
          target: request.targets[1],
          hint: { latestVersion: "2", isLatest: true, fetchedAt: 1 },
          error: null,
          origin: "cache" as const,
        },
      ];
      // The daemon streams one partial per target, then a final aggregate that
      // repeats every result.
      onPartial?.({
        version: request.version,
        request_id: request.request_id,
        results: [results[0]],
        indexes: [0],
        error: null,
        diagnostics: [],
      });
      onPartial?.({
        version: request.version,
        request_id: request.request_id,
        results: [results[1]],
        indexes: [1],
        error: null,
        diagnostics: [],
      });
      return Promise.resolve({
        version: request.version,
        request_id: request.request_id,
        results,
        error: null,
        diagnostics: [],
      });
    },
  };
  const refresher = new RegistryHintRefresher(daemon, harness.host, logger, () => true);

  await refresher.refresh(uriKey, [targetFor(a), targetFor(b)], "refresh_stale");

  const summaries = messages.filter((message) => message.includes("target(s):"));
  assert.equal(summaries.length, 1, `expected exactly one summary, got: ${messages.join(" | ")}`);
  assert.ok(summaries[0].includes("2 target(s): 1 cached, 1 fetched, 0 failed"));
  // Each package's verbose line appears once, not once per partial + once for final.
  assert.equal(messages.filter((message) => message.includes("fetched (network) for a")).length, 1);
});
