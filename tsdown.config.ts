import { defineConfig } from "tsdown";

export default defineConfig({
  entry: ["./extension/src/extension.ts"],
  format: ["cjs"],
  outDir: "./dist/extension",
  clean: true,
  minify: true,
  target: "node20",
  platform: "node",
  sourcemap: false,
  dts: false,
  deps: {
    alwaysBundle: ["@msgpack/msgpack"],
    neverBundle: ["vscode"],
    onlyBundle: ["@msgpack/msgpack"],
  },
  outputOptions: {
    entryFileNames: "extension.cjs",
  },
});
