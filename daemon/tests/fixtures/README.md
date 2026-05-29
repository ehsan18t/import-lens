# Package Fixtures

These package snapshots are committed as a single zip archive (`packages.zip`) so daemon integration
tests never read from the live npm registry while keeping the repository lean.
The archive contains multiple package workspaces, each with a minimal `package.json`, 
a `src/app.ts`, and a complete `node_modules` tree for one pinned package version.

The test harness extracts the archive on first use (via `fixture_workspace()`
in `analyze.rs`) into the `packages/` directory, which is gitignored.

Do not update these snapshots as drive-by dependency churn. Updating a fixture
requires regenerating expected size assertions in daemon integration tests.
