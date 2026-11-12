# Changelog

All notable changes to the `bonsai` CLI are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.0] - 2026-11-12

### Changed

- **Breaking:** `bonsai cluster url <cluster> --json` prints an object, `{"url": "…"}`, and with `--env NAME` also `"variable": "NAME"`. It used to print the same bare line as without `--json`.

### Fixed

- `cluster url --write` and the MCP tool `write_cluster_url` check the file's literal name against git. A name git reads as pathspec magic, such as `:(top).env`, no longer passes for the ignored `.env`.
- Both write nothing when the API's URL has a space or a control character in it, so a line break can't add lines to the env file.
- `bonsai skill install --project` refuses to write through a symbolic link, whether it is `.claude`, a folder below it or `SKILL.md` itself.
- The config folder is private (mode 0700) when the update check is what creates it, and one an earlier release left open to the group or to others is tightened. The update check's state file is mode 0600.
- A response larger than 64 MiB is refused instead of read into memory.
- MCP tool results escape control, bidi and invisible characters in the JSON they return. It parses to the same value.
- The MCP tools' parameter text names the values the API takes: the state `ready`, the resolutions `1m`, `30m`, `1h`, `6h`, `12h` and `1d`, and the windows `1h`, `1d`, `7d`, `14d` and `30d`.
- A value the API refuses for a flag, such as `--resolution 5m` or `--window 3d`, exits 2 (usage) instead of 1.
- An empty `BONSAI_CLUSTER` counts as no cluster.
- When `cluster wait` gives up, it reports the whole `--timeout`, not the part of it the last Action was given.
- Commands that a message suggests, such as `bonsai wait <id>`, quote the ID as one shell word.
- Tables fit `COLUMNS` only on a terminal. Piped output is whole, even in a shell that exports `COLUMNS`.
- A browser login is polled for an hour at most, whatever lifetime the API gives it.
- In `key  value` output, a value with a newline in it stays on one row.

## [0.3.1] - 2026-11-09

### Changed

- Fewer API requests per command. A cluster named by its ID costs one request with `--json` (`cluster usage`, `metrics`, `queries`, `history`) and with `cluster url`, instead of two; the cluster is looked up by name only when that request is a 404.
- Human output of `cluster usage`, `metrics`, `queries` and `history` fetches the cluster at the same time as the data, instead of before it.
- A cluster found by name is no longer fetched a second time.

## [0.3.0] - 2026-11-09

### Changed

- **Breaking:** cluster commands live only under `bonsai cluster`. The old top-level forms are gone and exit 2:

  | Before | Now |
  |---|---|
  | `bonsai clusters` | `bonsai cluster list` |
  | `bonsai history <cluster>` | `bonsai cluster history <cluster>` |
  | `bonsai url <cluster>` | `bonsai cluster url <cluster>` |
  | `bonsai usage <cluster>` | `bonsai cluster usage <cluster>` |
  | `bonsai metrics <cluster>` | `bonsai cluster metrics <cluster>` |
  | `bonsai queries <cluster>` | `bonsai cluster queries <cluster>` |

  `bonsai wait` and `bonsai action` are unchanged. Run `bonsai skill install` again to update the Claude Code skill.
- **Breaking:** the MCP tools are named after the API operations they call: `usage` is `get_cluster_usage`, `metrics` is `get_cluster_metrics`, `top_queries` is `get_cluster_top_queries`, and `cluster_history` is `list_cluster_actions`.
- `cluster metrics` shows latency as `typical` (the median bucket) and `in the slowest 1m`, instead of "(worst bucket)".
- `cluster usage` and `cluster metrics` show the cluster's name and ID on separate, labelled rows; `cluster queries` labels the ID too.
- `whoami` labels the token key as `Token key (public)`.

### Added

- `bonsai regions --plan <plan>` lists the regions where a plan is offered.
- `bonsai plans` shows a REGIONS column unless `--region` is given.
- With `-v`, looking a cluster up by name says so, so the 404 from trying it as an ID doesn't read as an error.

## [0.2.1] - 2026-11-09

### Added

- `bonsai url <cluster> --write .env` sets `BONSAI_URL` (or `--env NAME`) in the file without printing the password. It replaces any old line, leaves the file with mode 0600, and refuses a file outside the current folder or one that git doesn't ignore or that is tracked.

### Changed

- The skill tells agents to save the URL with `--write`, or with the MCP tool `write_cluster_url`, and never to print it.
- The docs use `BONSAI_URL` instead of `ELASTICSEARCH_URL`, as the MCP server and Bonsai's Heroku add-on do.

## [0.2.0] - 2026-11-09

### Added

- `bonsai mcp`, an MCP server on stdio for Claude Code and other MCP clients: `claude mcp add bonsai -- bonsai mcp`. It uses the CLI's token, returns the API's JSON, and never returns a password: `write_cluster_url` writes the connection URL to a git-ignored env file instead. `destroy_cluster` needs the cluster's current name as `confirm`, and `--read-only` leaves out the tools that change clusters.

## [0.1.0] - 2026-11-05

### Added

- First pre-release: browser and token login, `whoami`, `doctor`, creating, showing, renaming, resizing and destroying clusters, connection URLs, usage, metrics, top search terms, Actions, the plan, region, space and release catalogue, `bonsai api`, shell completions and the Claude Code skill.

[Unreleased]: https://github.com/omc/bonsai-cli/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/omc/bonsai-cli/compare/v0.3.1...v0.4.0
[0.3.1]: https://github.com/omc/bonsai-cli/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/omc/bonsai-cli/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/omc/bonsai-cli/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/omc/bonsai-cli/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/omc/bonsai-cli/releases/tag/v0.1.0
