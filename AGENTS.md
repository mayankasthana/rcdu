# AGENTS.md

Instructions for coding agents working on this repository.

## Workflow

- Always work in a git worktree, never directly in an existing checkout:
  `git worktree add ../rcdu-<topic> -b <branch>` — then work inside that directory.
- Do all feature and bug-fix work on feature branches. Never commit directly to `main`.
- Branch names should say what they are: `feature/<topic>` for features, `fix/<topic>` for bug fixes.
- Make small, incremental commits. Each commit should compile and pass `cargo test` on its own — never leave the tree broken mid-series.
- Write good commit messages: imperative mood, concise subject line (≤ 72 chars), e.g. "Add o key to open the selected entry with the system handler". Use the body to explain the *why* when it isn't obvious from the diff.
- One logical change per branch. Don't mix unrelated fixes or refactors into a feature branch.

## Merging

- Merge PRs only when GitHub CI is fully green on the PR. Never merge with failing or still-running checks.
- Re-run CI (or push an empty commit) if checks are stale relative to the latest commit.
- Never force-push shared branches.

## no-mistakes

- Use the no-mistakes pipeline to validate changes (review, tests, lint, push, PR) whenever it is available for the change at hand.
- `origin` is the GitHub remote (PRs and CI live there). The `no-mistakes` remote is internal to the pipeline — do not push to it manually.

## Quality gates (must pass before pushing — these are exactly what CI runs)

- `cargo fmt --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test`
- `cargo build --release`

CI runs all of these on both `ubuntu-latest` and `macos-latest`. A change that only builds or passes on one platform is not done.

## Tests

- Add unit tests for every change. Bug fixes get a test that reproduces the bug (red) before the fix (green); new features get tests covering the new behavior and its edge cases.
- Tests live in `#[cfg(test)]` modules at the bottom of each source file, matching the existing layout in `src/`.
- If a change is hard to unit test (e.g. terminal rendering), say so explicitly in the PR description and verify it manually instead.

## Code

- Keep changes minimal and focused. Fix the problem at hand; no drive-by refactors or reformatting of unrelated code.
- Match the style of the surrounding code.
- Avoid adding new dependencies. If one is genuinely needed, justify it in the commit message and PR description.
- Do not bump the version in `Cargo.toml` or touch `Cargo.lock` unless the task is a release.
