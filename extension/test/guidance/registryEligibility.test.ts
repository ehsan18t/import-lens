import assert from "node:assert/strict";
import test from "node:test";
import {
  privateRegistryScopes,
  type RegistryTargetState,
  withRegistryEligibility,
} from "../../src/guidance/registryEligibility.js";
import { registryTargetsForStates } from "../../src/guidance/registryRefresh.js";

const stateFor = (
  name: string,
  spec: string,
  overrides: Partial<RegistryTargetState> = {},
): RegistryTargetState => ({
  name,
  section: "dependencies",
  status: "ready",
  installedVersion: "1.0.0",
  entry: { version: spec },
  ...overrides,
});

test("registryTargetsForStates sends only public registry dependencies to the daemon", () => {
  const states = [
    stateFor("react", "^19.0.0"),
    stateFor("zod", "catalog:"),
    stateFor("lodash", "latest"),
    stateFor("@acme/ui", "workspace:*"),
    stateFor("local", "file:../local"),
    stateFor("linked", "link:../linked"),
    stateFor("portal", "portal:../portal"),
    stateFor("git", "git+https://github.com/owner/git.git"),
    stateFor("gh", "github:owner/gh"),
    stateFor("shorthand", "owner/shorthand"),
    stateFor("tarball", "https://example.com/tarball.tgz"),
    stateFor("std", "jsr:@std/path@1"),
    stateFor("foo", "npm:bar@1"),
    stateFor("@private/lib", "^1.0.0"),
    stateFor("@public/lib", "^1.0.0"),
  ];

  const names = registryTargetsForStates(states, new Set(["@private"])).map(
    (target) => target.name,
  );

  assert.deepEqual(names, ["react", "zod", "lodash", "@public/lib"]);
});

test("privateRegistryScopes keeps scopes mapped away from the public registry, later files winning", () => {
  const user = [
    "; user config",
    "@corp:registry=https://npm.corp.example/",
    "@back:registry=https://npm.corp.example/",
    "registry=https://registry.npmjs.org/",
  ].join("\n");
  const project = [
    "# project config",
    '@gh:registry = "https://npm.pkg.github.com"',
    "@back:registry=https://registry.npmjs.org/",
    "@public:registry=https://registry.npmjs.org/",
    `@env:registry=\${PRIVATE_REGISTRY}`,
    "//npm.corp.example/:always-auth=true",
  ].join("\r\n");

  assert.deepEqual([...privateRegistryScopes([user, project])].sort(), ["@corp", "@env", "@gh"]);
});

test("withRegistryEligibility clears a cached hint for a dependency that is not looked up", () => {
  const hint = { latestVersion: "2.0.0", isLatest: false, fetchedAt: 1 };
  const [workspace, registry] = withRegistryEligibility(
    [
      stateFor("@acme/ui", "workspace:*", { registryHint: hint }),
      stateFor("react", "^19.0.0", { registryHint: hint }),
    ],
    new Set(),
  );

  assert.equal(workspace?.registryHint, null);
  assert.equal(workspace?.registryLookup, false);
  assert.equal(registry?.registryHint, hint);
  assert.equal(registry?.registryLookup, true);
});
