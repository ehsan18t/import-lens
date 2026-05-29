# ImportLens

ImportLens is a VS Code extension that displays npm import size estimates inline while editing JavaScript, TypeScript, JSX/TSX, Svelte, and Astro files.

The default display mode uses VS Code inlay hints. Users can switch `importLens.display` to `standard` or `verbose` for end-of-line decorations, but inlay hints are preferred for accessibility because VS Code exposes them through the document model while decorations are visual-only.

When a package size is unavailable, hover the `unavailable` hint and use `Copy diagnostics` to copy the daemon's structured error context for debugging.

Svelte support analyzes imports inside `<script>` blocks. Astro support analyzes frontmatter imports as server-side runtime and processed client `<script>` blocks as client runtime; server-side results are labeled with `server` in the size hint.
