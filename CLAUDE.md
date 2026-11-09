# Commits

This project uses [Conventional Commits](https://www.conventionalcommits.org/), overriding the global no-prefix rule. release-plz reads the type to pick the version bump and group the changelog.

- Title only, no body: `<type>: <summary>`, lowercase summary, no trailing period.
- Types: `feat` (new behaviour), `fix` (bug fix), `perf`, `refactor`, `docs`, `test`, `ci`, `build`, `chore`.
- A breaking change to the CLI (removed or renamed command, flag, output field, exit code) adds `!`: `feat!: drop --json-legacy`.
- One logical change per commit, ordered for commit-by-commit review.

# Releases

1. From a local checkout on an up-to-date `main`, `release-plz release-pr --git-token "$(gh auth token)"` opens the release PR (version bump, `CHANGELOG.md`).
2. Review and merge it.
3. Once `ci` is green on that commit, `gh workflow run release.yml -f tag=v<version>` starts `.github/workflows/release.yml` (dist) on `main`: it builds the macOS and Linux binaries, checks that `ci` passed (`ci-green.yml`), then creates the `v<version>` tag and the GitHub Release with the binaries attached. If `ci` hasn't passed, nothing is tagged. A tag that doesn't match the version in `Cargo.toml` stops the run at the plan job.

`gh workflow run release.yml` with no tag is a rehearsal: it does everything but create the tag and the Release. Pushing a tag by hand doesn't start a release. Nothing goes to Homebrew or npm; `dist-workspace.toml` says what turns them back on.

`release.yml` is generated: edit `dist-workspace.toml`, then run `dist generate` (dist 0.33.0). `flake.nix` builds from source and needs no release step.

Releases target macOS and Linux only; there is no Windows build.
