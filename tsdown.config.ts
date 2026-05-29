import { defineConfig } from "tsdown";

export default defineConfig({
  entry: ["./extension/src/extension.ts"],
  format: ["cjs"],
  outDir: "./extension/dist",
  clean: true,
  minify: true,
  sourcemap: false,
  dts: false,
  external: ["vscode"],
  outputOptions: {
    entryFileNames: "extension.cjs",
  },
});

