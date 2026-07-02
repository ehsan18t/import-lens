import assert from "node:assert/strict";
import test from "node:test";
import type {
  RefreshRegistryHintsRequest,
  RefreshRegistryHintsResponse,
  RegistryHintTarget,
} from "../../src/ipc/protocol.js";
import type { PackageJsonDependencyHintState } from "../../src/guidance/packageJsonState.js";
import {
  RegistryHintRefresher,
  type RegistryRefreshHost,
  type RegistryRefreshTransport,
} from "../../src/guidance/registryRefresh.js";

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
        target: request.targets[0]!,
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
