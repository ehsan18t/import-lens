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

