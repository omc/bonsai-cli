---
name: bonsai
description: Manage Bonsai (managed Elasticsearch and OpenSearch) clusters with the `bonsai` CLI. Use when asked to create, resize, rename or destroy a Bonsai cluster, get its connection URL, check its usage, metrics or top search queries, or look up Bonsai plans, regions and releases.
---

# bonsai

`bonsai` is the Bonsai CLI, installed on this machine. Run it through the shell.

## Always

- Add `--json --no-input` to every command. `--json` prints the API's JSON on stdout. `--no-input` makes a missing value fail with exit 2, naming the flag, instead of waiting on a prompt nobody will answer.
- Check the exit code and stderr, not only stdout. The codes are below.
- Run `bonsai <command> --help` when unsure of a flag instead of guessing.

## First

Run `bonsai whoami --json`. Exit 3 means there's no valid token: ask the user to run `bonsai login` in their own terminal (it opens a browser), or to set `BONSAI_TOKEN`. Never ask for a token in the chat.

`bonsai doctor --json` checks the API, the token, the contract version and the clock.

## Commands

| Task | Command |
|---|---|
| List clusters | `bonsai clusters --json` (filters: `--state`, `--region`, `--name`) |
| One cluster | `bonsai cluster show <cluster> --json` |
| What can be created | `bonsai plans --region <region> --json`, `bonsai regions --json`, `bonsai releases --json`, `bonsai spaces --json` |
| Create | `bonsai cluster create <name> --plan <plan> --region <region> --json --no-input` |
| Rename | `bonsai cluster rename <cluster> <new-name> --json --no-input` |
| Change plan | `bonsai cluster resize <cluster> <plan> --json --no-input` |
| Destroy | `bonsai cluster destroy <cluster> --confirm <current-name> --json --no-input` |
| Connection URL, into the app's env file | `bonsai url <cluster> --write .env --json --no-input` (see Secrets) |
| Docs, shards and size against the plan's limits | `bonsai usage <cluster> --since 7d --json` |
| Request rates, errors, latency | `bonsai metrics <cluster> --since 1h --json` |
| Top search terms | `bonsai queries <cluster> --json` |
| Past changes | `bonsai history <cluster> --json`, `bonsai action show <action-id> --json` |
| Anything without a command | `bonsai api GET /clusters --paginate` |

`<cluster>` is an ID or a name. A name two clusters share exits 2; use the ID then. `bonsai api` paths take IDs, not names.

`bonsai cluster create --json` prints the finished Action; the new cluster's ID is `.result.cluster`.

## Changes cost money

Confirm with the user before `cluster create`, `cluster resize` and `cluster destroy`, naming the cluster, plan and region. Take plan and region IDs from `bonsai plans --json` and `bonsai regions --json`, never from memory. A destroyed cluster can't be brought back.

## Slow changes are Actions

Create, resize and destroy start an Action on Bonsai and wait for it, 30 minutes at most by default (`--timeout`). `--no-wait` prints the Action and returns at once; `bonsai wait <action-id>` waits again. Exit 9 means the wait ran out, not that the change stopped: run `bonsai wait <action-id>`. Exit 7 means another change is already running on that cluster.

## Secrets

`bonsai url` prints the cluster's password: never run it without `--write`. `bonsai api` prints whatever the API returns, which can include passwords. Never repeat a password in the chat, commits or logs.

Write the URL straight into the file the app reads, with the bonsai MCP server's `write_cluster_url` tool if it's available, or:

```sh
bonsai url <cluster> --write .env --json --no-input
```

It sets `BONSAI_URL` (`--env NAME` for another variable), replacing any old line, and answers with the password as `****`. It writes only inside the current folder, leaves the file readable only by its owner, and refuses (exit 2) a file git doesn't ignore or that is tracked: then ask the user to add it to `.gitignore` instead of working around it.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | Network or server error, anything not listed below |
| 2 | Usage: a bad or missing flag, or an ambiguous cluster name |
| 3 | Not allowed: no token, an invalid token, or a missing scope |
| 4 | Not found |
| 5 | The Action failed |
| 6 | A payment method is needed, or the card was refused; tell the user |
| 7 | Busy: another change is already running |
| 8 | Refused: the request was valid but Bonsai declined it |
| 9 | `--timeout` ran out before the Action finished |
| 130 | Interrupted |
