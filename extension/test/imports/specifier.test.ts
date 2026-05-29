import assert from "node:assert/strict";
import test from "node:test";
import { getPackageName, isRuntimePackageSpecifier } from "../../src/imports/specifier.js";

test("getPackageName extracts root names from bare, subpath, and scoped specifiers", () => {
  assert.equal(getPackageName("react"), "react");
  assert.equal(getPackageName("date-fns/format"), "date-fns");
  assert.equal(getPackageName("@babel/core/lib/parser"), "@babel/core");
});

test("isRuntimePackageSpecifier rejects relative paths and Node builtins", () => {
  assert.equal(isRuntimePackageSpecifier("./local"), false);
  assert.equal(isRuntimePackageSpecifier("../parent"), false);
  assert.equal(isRuntimePackageSpecifier("node:fs"), false);
  assert.equal(isRuntimePackageSpecifier("fs"), false);
  assert.equal(isRuntimePackageSpecifier("lodash-es"), true);
});

test("isRuntimePackageSpecifier rejects framework virtual and app alias imports", () => {
  assert.equal(isRuntimePackageSpecifier("astro:content"), false);
  assert.equal(isRuntimePackageSpecifier("virtual:astro/icons"), false);
  assert.equal(isRuntimePackageSpecifier("$app/environment"), false);
  assert.equal(isRuntimePackageSpecifier("$env/static/public"), false);
  assert.equal(isRuntimePackageSpecifier("$lib/server/config"), false);
  assert.equal(isRuntimePackageSpecifier("#imports"), false);
  assert.equal(isRuntimePackageSpecifier("@/components/Button"), false);
  assert.equal(isRuntimePackageSpecifier("~/components/Button"), false);
});

test("isRuntimePackageSpecifier rejects host-provided runtime modules", () => {
  assert.equal(isRuntimePackageSpecifier("vscode"), false);
  assert.equal(isRuntimePackageSpecifier("electron"), false);
  assert.equal(isRuntimePackageSpecifier("bun:test"), false);
});
