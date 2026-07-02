import {
  protocolVersion,
  type RefreshRegistryHintsRequest,
  type RefreshRegistryHintsResponse,
  type RegistryHint,
  type RegistryHintTarget,
} from "../ipc/protocol.js";
import { nextIpcRequestId } from "../ipc/requestIds.js";
import type { Logger } from "../logging/types.js";
import { newerRegistryHint } from "./packageJsonPartial.js";
import type { PackageJsonDependencyHintState, RegistryHintRefreshStatus } from "./packageJsonState.js";

export interface RegistryRefreshTransport {
  refreshRegistryHints(
    request: RefreshRegistryHintsRequest,
    onPartial?: (response: RefreshRegistryHintsResponse) => void,
  ): Promise<RefreshRegistryHintsResponse | null>;
}

export interface RegistryRefreshHost<TUri, TState extends PackageJsonDependencyHintState> {
  keyFor(uri: TUri): string;
  getStates(uri: TUri): readonly TState[] | undefined;
  setStates(uri: TUri, states: TState[]): void;
}

export class RegistryHintRefresher<TUri, TState extends PackageJsonDependencyHintState> {
  readonly #daemon: RegistryRefreshTransport;
  readonly #host: RegistryRefreshHost<TUri, TState>;
  readonly #logger: Pick<Logger, "debug" | "warn">;
  readonly #generations = new Map<string, number>();
  #nextGeneration = 0;

  constructor(
    daemon: RegistryRefreshTransport,
    host: RegistryRefreshHost<TUri, TState>,
    logger: Pick<Logger, "debug" | "warn">,
  ) {
    this.#daemon = daemon;
    this.#host = host;
    this.#logger = logger;
  }

  async refresh(
    uri: TUri,
    targets: readonly RegistryHintTarget[],
    mode: RefreshRegistryHintsRequest["mode"],
  ): Promise<void> {
    const generation = this.#beginGeneration(this.#host.keyFor(uri));
    const pendingTargets = registryTargetMap(targets);
    const markCompleted = (response: RefreshRegistryHintsResponse): void => {
      for (const result of response.results) {
        pendingTargets.delete(registryTargetKey(result.target));
      }
    };

    try {
      const response = await this.#daemon.refreshRegistryHints({
        type: "refresh_registry_hints",
        version: protocolVersion,
        request_id: nextIpcRequestId(),
        targets: [...targets],
        mode,
      }, (partial) => {
        markCompleted(partial);
        this.#applyResponse(uri, generation, partial);
      });

      if (!response) {
        this.#applyRequestFailure(
          uri,
          generation,
          [...pendingTargets.values()],
          new Error("Daemon unavailable"),
        );
        return;
      }

      markCompleted(response);
      this.#applyResponse(uri, generation, response);

      if (response.error && pendingTargets.size > 0) {
        this.#applyRequestFailure(
          uri,
          generation,
          [...pendingTargets.values()],
          new Error(response.error),
        );
      }
    } catch (error) {
      this.#applyRequestFailure(uri, generation, [...pendingTargets.values()], error);
    }
  }

  forget(uri: TUri): void {
    this.#generations.delete(this.#host.keyFor(uri));
  }

  #beginGeneration(key: string): number {
    const generation = ++this.#nextGeneration;
    this.#generations.set(key, generation);
    return generation;
  }

  #isSuperseded(uri: TUri, generation: number): boolean {
    return generation < (this.#generations.get(this.#host.keyFor(uri)) ?? 0);
  }

  #applyResponse(uri: TUri, generation: number, response: RefreshRegistryHintsResponse): void {
    if (response.error) {
      this.#logger.debug(`Registry hint refresh response failed: ${response.error}`);
    }

    for (const result of response.results) {
      if (result.error) {
        this.#logger.debug(`Registry hint unavailable for ${result.target.name}: ${result.error}`);
      }
      this.#updateRegistryHint(
        uri,
        generation,
        result.target.name,
        result.target.installedVersion,
        result.hint ?? undefined,
        result.error ?? null,
      );
    }
  }

  #applyRequestFailure(
    uri: TUri,
    generation: number,
    targets: readonly RegistryHintTarget[],
    error: unknown,
  ): void {
    const message = error instanceof Error ? error.message : String(error);
    this.#logger.warn(`Registry hint refresh request failed: ${message}`);

    for (const target of targets) {
      this.#updateRegistryHint(
        uri,
        generation,
        target.name,
        target.installedVersion,
        undefined,
        message,
      );
    }
  }

  #updateRegistryHint(
    uri: TUri,
    generation: number,
    packageName: string,
    installedVersion: string | undefined,
    hint: RegistryHint | null | undefined,
    refreshError: string | null,
  ): void {
    const states = this.#host.getStates(uri);

    if (!states) {
      return;
    }

    const superseded = this.#isSuperseded(uri, generation);
    let changed = false;
    const nextStates = states.map((state) => {
      if (state.name !== packageName || state.installedVersion !== installedVersion) {
        return state;
      }

      const registryHint = newerRegistryHint(state.registryHint, hint);
      const registryHintRefreshStatus: RegistryHintRefreshStatus | undefined = superseded
        ? state.registryHintRefreshStatus
        : refreshError && registryHint
          ? "stale"
          : registryHint
            ? "fresh"
            : undefined;
      const registryHintRefreshError = superseded ? state.registryHintRefreshError : refreshError;

      if (
        registryHint === state.registryHint
        && registryHintRefreshStatus === state.registryHintRefreshStatus
        && registryHintRefreshError === state.registryHintRefreshError
      ) {
        return state;
      }

      changed = true;
      return {
        ...state,
        registryHint,
        registryHintRefreshStatus,
        registryHintRefreshError,
      } as TState;
    });

    if (changed) {
      this.#host.setStates(uri, nextStates);
    }
  }
}

export const registryTargetsForStates = (
  states: readonly PackageJsonDependencyHintState[],
): RegistryHintTarget[] => {
  const seen = new Set<string>();
  const targets: RegistryHintTarget[] = [];

  for (const state of states) {
    const key = registryTargetKey(state);

    if (seen.has(key)) {
      continue;
    }

    seen.add(key);
    targets.push({
      name: state.name,
      installedVersion: state.installedVersion,
    });
  }

  return targets;
};

const registryTargetMap = (
  targets: readonly RegistryHintTarget[],
): Map<string, RegistryHintTarget> =>
  new Map(targets.map((target) => [registryTargetKey(target), target]));

const registryTargetKey = (
  target: Pick<RegistryHintTarget, "name" | "installedVersion">,
): string => `${target.name}\n${target.installedVersion ?? ""}`;
