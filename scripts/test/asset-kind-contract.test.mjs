import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

// Drift check. The daemon's `AssetKind` and the extension's mirror are separately maintained records
// of the SAME wire vocabulary: the daemon serializes the enum snake_case into `asset_breakdown`, and
// the extension string-matches those spellings to label them. Nothing in either language links the
// two, so add a variant on one side only and everything still compiles — the hover just renders
// `- undefined: 4.2 kB` for the kind it has no label for, while the total stays right, which is
// exactly the kind of silent half-wrong surface a type checker cannot see.
//
// The expectation is DERIVED from the daemon's enum rather than typed out here, so this fails when
// the two move apart and never merely because someone edited a file.

const repoFile = (relativePath) =>
  readFileSync(new URL(`../../${relativePath}`, import.meta.url), "utf8");

const daemonAssetKinds = () => {
  const source = repoFile("daemon/src/engine/mod.rs");
  const body = source.match(/pub enum AssetKind \{([^}]*)\}/u)?.[1];
  assert.ok(body, "daemon `pub enum AssetKind` not found; this check is stale");

  const kinds = [...body.matchAll(/^\s*([A-Z][A-Za-z0-9]*),/gmu)].map((match) =>
    match[1].toLowerCase(),
  );
  assert.ok(kinds.length > 0, "no AssetKind variants parsed");
  return kinds.sort();
};

test("the extension's asset-kind union matches the daemon's enum", () => {
  const source = repoFile("extension/src/ipc/protocol.ts");
  const union = source.match(/export type AssetKind =([^;]*);/u)?.[1];
  assert.ok(union, "extension `AssetKind` union not found; this check is stale");

  const spellings = [...union.matchAll(/"([a-z0-9_]+)"/gu)].map((match) => match[1]).sort();

  assert.deepEqual(
    spellings,
    daemonAssetKinds(),
    "the daemon serializes AssetKind snake_case into `asset_breakdown`; a variant the extension's " +
      "union does not know is a value it cannot render",
  );
});

test("every asset kind the daemon can send has a label to render", () => {
  const source = repoFile("extension/src/ui/format.ts");
  const body = source.match(/assetKindLabels[^{]*\{([^}]*)\}/u)?.[1];
  assert.ok(body, "`assetKindLabels` not found; this check is stale");

  const labelled = [...body.matchAll(/^\s*([a-z0-9_]+):/gmu)].map((match) => match[1]).sort();

  assert.deepEqual(
    labelled,
    daemonAssetKinds(),
    "a kind with no label renders as `- undefined: 4.2 kB` in the hover while the total stays right",
  );
});
