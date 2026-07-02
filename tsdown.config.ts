import { defineConfig } from "tsdown";

export default defineConfig({
  entry: ["./extension/src/extension.ts"],
  format: ["cjs"],
  outDir: "./extension/dist",
  clean: true,
  minify: true,
  target: "node20",
  platform: "node",
  sourcemap: false,
  dts: false,
  deps: {
    alwaysBundle: ["@msgpack/msgpack"],
    neverBundle: ["vscode", "oxc-parser"],
    onlyBundle: ["@msgpack/msgpack"],
  },
  outputOptions: {
    entryFileNames: "extension.cjs",
  },
});
