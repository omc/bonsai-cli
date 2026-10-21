# Commits

This project uses [Conventional Commits](https://www.conventionalcommits.org/), overriding the global no-prefix rule. release-plz reads the type to pick the version bump and group the changelog.

- Title only, no body: `<type>: <summary>`, lowercase summary, no trailing period.
- Types: `feat` (new behaviour), `fix` (bug fix), `perf`, `refactor`, `docs`, `test`, `ci`, `build`, `chore`.
- A breaking change to the CLI (removed or renamed command, flag, output field, exit code) adds `!`: `feat!: drop --json-legacy`.
- One logical change per commit, ordered for commit-by-commit review.

# Releases

Run from a local checkout on an up-to-date `main`, never from CI:

1. `release-plz release-pr --git-token "$(gh auth token)"` opens the release PR (version bump, `CHANGELOG.md`).
2. Review and merge it.
3. `git pull`, then `release-plz release --git-token "$(gh auth token)"` pushes the `v<version>` tag.
4. The tag starts `.github/workflows/release.yml` (dist): builds the macOS and Linux binaries, creates the GitHub Release, updates the Homebrew tap, publishes `@omc/bonsai` to npm.

`release.yml` is generated: edit `dist-workspace.toml`, then run `dist generate` (dist 0.33.0). `flake.nix` builds from source and needs no release step.

Releases target macOS and Linux only; there is no Windows build.
