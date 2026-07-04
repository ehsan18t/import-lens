# AGENTS.md

Project instructions for agents working in this repository.

## Project Context

- ImportLens is a VS Code extension with a TypeScript extension host and a Rust daemon.
- Use `pnpm` as the npm package manager. Do not use `npm` or `yarn` for project scripts or dependency changes.
- Windows is the primary supported platform right now. Keep Windows compilation and packaging working before broadening to other targets.
- The SRS is the source of truth for intended behavior: `docs/ImportLens-SRS.md`.

## File And Formatting Rules

- Keep files in LF line endings. Never save files as CRLF.
- Keep edits scoped to the task. Do not mix unrelated refactors into feature or fix work.
- Keep types, constants, helpers, utilities, and UI code in their appropriate modules. Do not bury shared logic in unrelated files.
- Prefer existing patterns and helper modules before creating new abstractions.
- In TypeScript, prefer arrow functions where practical.
- Do not use double casting or unnecessary cast chains.

## Implementation Workflow

- Read the relevant existing code before editing.
- Add or update tests for behavior changes and bug fixes.
- For daemon changes, run Rust formatting and tests.
- For extension changes, run TypeScript checks and tests.
- If behavior diverges from the SRS, update `docs/ImportLens-SRS.md` in the same task.
- If daemon code changes, rebuild/package for Windows and refresh the daemon hash before handing off.
- Don't add unnecessary tests.
- Don't put something I give you as future or milestone or deferred work. Because if I give some you to do, I am asking you to do it right now.
- Don't update docs inside the superpower sub-directory unless it's something that has not been implemented yet.
- Split work into tasks.

## Verification Commands

Use the narrowest relevant checks while developing, then run the full set before completion:

```powershell
pnpm check
pnpm test
cargo fmt --check
pnpm package:win32-x64
```

## Packaging Notes

- `pnpm package:win32-x64` rebuilds the daemon, copies the Windows binary, refreshes `extension/src/daemon/knownHashes.generated.ts`, builds the extension bundle, and creates the Windows VSIX.
- Generated build artifacts such as `dist/` (daemon binaries, extension bundle, VSIXes) and `target/` are ignored unless the repository policy changes.

## Git Expectations

- Keep commits focused by task or feature.
- Use professional commit messages with enough body detail to explain the user-visible change and important technical rationale.
- Do not revert user changes unless explicitly asked.
- Before committing, check `git status --short` and review the staged diff.
