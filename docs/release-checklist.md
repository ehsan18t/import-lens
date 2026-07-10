# Release Checklist

A quick tick-box to run through for every Import Lens release. For the *how* and the *one-time
setup*, see the [Release Setup Guide](release-setup-guide.md) — this page is just the gate.

## Before you start

- [ ] `package.json` `version` bumped and committed to `main` (e.g. `0.1.0` → `0.2.0`) — the normal
      path is to leave the workflow `version` blank and let it read this. (You *can* instead type a
      `version` into the workflows to override `package.json` for that run.)
- [ ] `main` is in the exact state you want to ship (the release builds from the latest commit).
- [ ] `media/icon.png` exists and is the final marketplace icon.
- [ ] (Optional local sanity) from a clean checkout: `pnpm install --frozen-lockfile`, `pnpm check`,
      `pnpm test`.

## Build stage — run the **Build** workflow

- [ ] Ran **Build** — `version` left blank to use `package.json`, or a value typed to override it
      (no leading `v`).
- [ ] All **6** platform jobs are green **in a single run**: `win32-x64`, `win32-arm64`,
      `linux-x64`, `linux-arm64`, `darwin-x64`, `darwin-arm64`.
- [ ] If any platform failed, re-ran Build (same version) until all 6 succeeded — cached targets
      skip instantly, only failures rebuild.
- [ ] Each VSIX passed the 20 MB size gate (the Build job enforces this).

## Release stage — dry run first

- [ ] Ran **Release** with **dry_run checked** and the destinations you intend to publish to.
- [ ] Dry run confirmed: all 6 artifacts present, selected-store tokens configured (no preflight
      failure), and the **changelog preview looks correct**.

## Release stage — the real run

- [ ] Ran **Release** with **dry_run unchecked**.
- [ ] Selected the right destinations:
  - `release_github` (always on)
  - `publish_vscode` — only if publishing to the VS Code Marketplace
  - `publish_openvsx` — only if publishing to Open VSX
- [ ] Workflow succeeded; the draft GitHub release was created with all 6 VSIX files attached.

## Publish gate — do **not** publish the GitHub draft if…

- [ ] …any of the 6 target VSIXs is missing from the draft.
- [ ] …any VSIX exceeds 20 MB.
- [ ] …the release icon is a placeholder or missing.
- [ ] …the changelog is wrong or misleading (edit the draft's notes before publishing — this is the
      review safety net for AI-generated notes).

## Finish

- [ ] Reviewed and edited the draft release notes as needed.
- [ ] Clicked **Publish release** (this makes it public and creates the `vX.Y.Z` tag).
- [ ] Spot-checked that the selected stores show the new version (Marketplace / Open VSX may take a
      few minutes to index).
