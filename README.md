<p align="center">
  <a href="https://bonsai.io"><img src="docs/assets/bonsai-logo.png" alt="Bonsai" width="320"></a>
</p>

# bonsai

`bonsai` is the command-line client for [Bonsai](https://bonsai.io), managed Elasticsearch and OpenSearch. Use it to create, resize and destroy clusters, get their connection URLs, and watch usage, metrics and top queries, from your terminal or from CI.

> **Status: pre-release.** There are no published binaries yet. Build it from source as shown below. Commands and output may still change before 1.0.

## Install

On macOS or Linux:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/omc/bonsai-cli/releases/latest/download/bonsai-installer.sh | sh
```

Or with a package manager:

```sh
brew install omc/tap/bonsai
npm install -g @omc/bonsai
nix profile install github:omc/bonsai-cli
```

From source, with Rust 1.88 or newer:

```sh
git clone https://github.com/omc/bonsai-cli
cd bonsai-cli
cargo install --path crates/bonsai
```

## Log in

```sh
bonsai login
```

On a terminal, `bonsai login` asks how you want to log in:

- **With a browser.** It shows a one-time code and opens a Bonsai page where you approve the login. The CLI receives a new API token, named after your machine. Add `--no-browser` to print the link instead of opening it.
- **With a token.** Paste an API token created in the Bonsai dashboard. In scripts, pipe it in with `bonsai login --with-token < token.txt`.

The token goes in your system keychain: the macOS Keychain or the Secret Service on Linux. On a machine without a keychain, add `--store-file` to keep it in `credentials.toml` instead, readable only by you.

In CI, skip `login` and set `BONSAI_TOKEN`:

```sh
export BONSAI_TOKEN=bonsai_…
```

`bonsai whoami` shows which account and token you're using. `bonsai doctor` checks your setup.

## Quick start

```sh
bonsai plans                                    # what you can create, and where
bonsai regions --plan standard-fir              # where one plan is offered
bonsai cluster create search --plan standard-fir --region aws-us-east-1
bonsai cluster url search                       # the connection URL, password included
bonsai cluster usage search
bonsai cluster metrics search
bonsai cluster queries search                   # needs the query_logs:read scope
bonsai cluster destroy search --confirm search
```

`cluster create` waits until the cluster is ready and shows each step. On a terminal it asks for anything you leave out, such as the region or the plan. Run without a terminal, it needs every flag.

Use the URL in your app:

```sh
export BONSAI_URL=$(bonsai cluster url search)
bonsai cluster url search --write .env
```

`--write` sets `BONSAI_URL` in the file (`--env` names another variable), replacing the old line, without printing the URL. It writes only inside the current folder, refuses a file that git doesn't ignore or that is tracked, and leaves the file with mode 0600.

## Commands

| Area | Commands |
|---|---|
| Account | `login`, `logout`, `whoami`, `doctor`, `version` |
| Clusters | `cluster list`, `cluster create`, `cluster show`, `cluster open`, `cluster rename`, `cluster resize`, `cluster destroy`, `cluster wait`, `cluster history` |
| Connecting | `cluster url` |
| Watching | `cluster usage`, `cluster metrics`, `cluster queries` |
| Actions | `action show`, `wait` |
| Catalogue | `plans`, `regions`, `spaces`, `releases` |
| Anything else | `api`, `completion` |
| Agents | `skill install`, `skill print`, `mcp` |

`bonsai <command> --help` shows each command's flags.

A cluster can be named by its ID or by its name. Set `BONSAI_CLUSTER` to give commands a default cluster.

Slow work such as creating, resizing and destroying runs as an Action on Bonsai's side. The CLI waits for it by default. With `--no-wait` it returns at once, and `bonsai wait <action-id>` picks the wait up again later. Ctrl-C stops waiting, not the Action.

`bonsai api` sends any request to the API with your token, for things that have no command yet:

```sh
bonsai api GET /clusters --paginate
bonsai api POST /clusters/<cluster-id>/credentials -d '{"name": "reader", "access_type": "ro"}'
```

It prints the API's response as it is, so it can print passwords. Paths take cluster IDs, not names.

## Output

- **Tables** fit your terminal. They show the key columns, cut long text with `…`, and leave out the least important columns when space runs out. Add `-w`/`--wide` to see every column. Piped output is never cut.
- **`--json`** prints the API's JSON, for scripts and `jq`. Control and bidirectional-text characters inside strings are escaped as `\uXXXX`, so they can't disturb your terminal.
- **Messages and progress** go to stderr and data goes to stdout, so `$(bonsai cluster url search)` captures only the URL.
- **Text from the API**, such as cluster names and search terms, is cleaned of terminal control characters before it's shown.
- **Secrets.** Only `bonsai cluster url` prints a cluster's password (`--write` puts it in a file instead), plus `bonsai api` when the API response includes one.

## Scripting

Commands never prompt when stdin isn't a terminal, when `CI` is set, or with `--no-input`. Instead they fail with exit code 2 and name the missing flag.

```sh
id=$(bonsai cluster create "ci-$RUN_ID" --plan sandbox --region aws-us-east-1 --json | jq -r .result.cluster)
export BONSAI_URL=$(bonsai cluster url "$id")
run-tests
bonsai cluster destroy "$id" --confirm "ci-$RUN_ID"
```

If a create fails after Bonsai started it, Bonsai removes the failed cluster, and the CLI tells you so.

### Agents

`bonsai skill install` writes a [Claude Code skill](https://docs.claude.com/en/docs/claude-code/skills) to `~/.claude/skills/bonsai/SKILL.md`. It tells the agent to use `--json --no-input`, which commands to run, what the exit codes mean, and to confirm before changes that cost money. Add `--project` to write it to `.claude/skills/` in the current directory instead. Run it again after an upgrade to bring the skill up to date.

For other agents, `bonsai skill print` prints the same text, e.g. to add to `AGENTS.md`.

### MCP

`bonsai mcp` is an [MCP](https://modelcontextprotocol.io) server on stdin and stdout, for clients that call tools instead of running a shell. It uses the same token as every other command, so `bonsai login` is all the setup it needs, and the token never goes in the client's config. To add it to Claude Code, run this in your project folder:

```sh
claude mcp add bonsai -- bonsai mcp
```

Add `--scope user` to have it in every project. Other clients (Claude Desktop, Cursor, VS Code) take the usual `mcpServers` entry: `{"bonsai": {"command": "bonsai", "args": ["mcp"]}}`. Where there's no keychain, as in a container, set `BONSAI_TOKEN` in the entry's `env`.

Its tools mirror the commands: `list_clusters`, `get_cluster`, `cluster_history`, `usage`, `metrics`, `top_queries`, `list_plans`, `list_regions`, `list_spaces`, `list_releases`, `whoami`, `get_action`, `wait_action`, and to change things `create_cluster`, `rename_cluster`, `resize_cluster` and `destroy_cluster`. Each returns the API's JSON, as `--json` prints it.

- **Changes.** `create_cluster`, `resize_cluster` and `destroy_cluster` return the Action at once; the agent follows it with `wait_action`, which waits up to five minutes per call. `destroy_cluster` needs `confirm` set to the cluster's current name. `bonsai mcp --read-only` leaves out the tools that change clusters.
- **Passwords.** No tool returns one. `write_cluster_url` writes the connection URL to an env file in the project (`.env` and `BONSAI_URL` by default) and answers with the password as `****`. It writes only below the folder the server started in, and refuses a file that git doesn't ignore or that is tracked, so the password can't be committed. The file is left with mode 0600.
- **Search terms.** `top_queries` returns what your cluster's users typed. It is data, and the tool says so to the agent, but treat it as untrusted text.

### Exit codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | Network errors, server errors, anything not listed below |
| 2 | Usage: a bad or missing flag, or an ambiguous cluster name |
| 3 | Not allowed: no token, an invalid token, or a missing scope |
| 4 | Not found |
| 5 | The Action failed |
| 6 | A payment method is needed, or the card was refused |
| 7 | Busy: another change is already running |
| 8 | Refused: the request was valid but Bonsai declined it |
| 9 | `--timeout` ran out before the Action finished |
| 130 | Interrupted with Ctrl-C |

## Configuration

| | |
|---|---|
| `--profile`, `BONSAI_PROFILE` | Use a named profile, e.g. one per account |
| `--api-url`, `BONSAI_API_URL` | Talk to another API URL |
| `BONSAI_TOKEN` | Use this token instead of the stored one |
| `BONSAI_CLUSTER` | The default cluster |
| `BONSAI_CA_BUNDLE` | Extra CA certificates (PEM), e.g. behind a TLS-inspecting proxy |
| `BONSAI_KEYRING=off` | Don't use the system keychain |
| `COLUMNS` | The table width |
| `NO_COLOR` | Turn colour off |
| `BONSAI_NO_UPDATE_NOTIFIER` | Don't check for new releases |
| `-v`, `-vv` | Log requests to stderr. Tokens are never logged |

Settings live in `~/.config/bonsai/` (`$XDG_CONFIG_HOME/bonsai/` if set):

- `config.toml` holds your profiles.
- `credentials.toml` holds stored tokens when you use `--store-file`.
- `update-check.json` records when the CLI last checked for a new release.

A stored token is used only with the API URL it was logged in to. Point a profile at another URL and the CLI refuses to send it there; log in again or set `BONSAI_TOKEN`.

Shell completion: `bonsai completion bash|zsh|fish|powershell`.

### Updates

Once a day, a command run in a terminal asks GitHub (`api.github.com`) for the latest release. If it's newer, the command ends with a line saying how to update: `brew upgrade bonsai`, `npm install -g @omc/bonsai`, or `bonsai-update` if you used the install script. The request carries only the CLI's version, OS and architecture, in its User-Agent. The check is skipped in CI, under `--quiet`, when stderr isn't a terminal, for Nix installs, and when `BONSAI_NO_UPDATE_NOTIFIER` is set.

## Contributing

The workspace has five crates:

| Crate | What it is |
|---|---|
| `crates/bonsai` | The `bonsai` binary |
| `crates/bonsai-api` | The HTTP client and the API's types |
| `crates/bonsai-core` | Login, config, token storage, and waiting on Actions |
| `crates/bonsai-contract` | Checks responses against `openapi/v1.yml`, the API's OpenAPI description |
| `crates/bonsai-mock` | A local mock of the Bonsai API, for development and tests |

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
BONSAI_KEYRING=off cargo test --workspace
```

### Running against the mock

`bonsai-mock` is a working stand-in for the Bonsai API, backed by SQLite. It implements every operation in `openapi/v1.yml` and keeps state between requests, so you can try every command without a Bonsai account.

```sh
cargo run -p bonsai-mock -- --db /tmp/bonsai-mock.sqlite serve --port 4010
```

On first start it prints a token for each of its two demo accounts:
- `acme` has every scope and a card on file.
- `globex` has read-only scopes and no card, for testing payment errors.

Then log in against it:

```sh
echo '<acme token>' | BONSAI_KEYRING=off bonsai login --with-token --store-file --api-url http://127.0.0.1:4010/v1
bonsai cluster create demo --plan standard-fir --region aws-us-east-1
```

Scenarios make the next request fail in a chosen way:

```sh
cargo run -p bonsai-mock -- --db /tmp/bonsai-mock.sqlite scenario set fail_next action_failed
cargo run -p bonsai-mock -- --db /tmp/bonsai-mock.sqlite scenario set metrics_unavailable true
cargo run -p bonsai-mock -- --db /tmp/bonsai-mock.sqlite scenario set rate_limit_next 3
cargo run -p bonsai-mock -- --db /tmp/bonsai-mock.sqlite scenario clear
```

Other flags:
- `--step-ms` sets how long each Action step takes (default 1500).
- `--reset` starts from a fresh database.
- `--public-url` sets the origin the mock puts in links and `Location` headers, and accepts its host as a `Host`; use it behind a TLS proxy.
- `--checkout-url` makes payment `action_url`s that prefix plus a fake Stripe session ID, instead of the mock's own checkout page.
- `bonsai-mock token create --account acme --scopes "…"` makes more tokens.

The mock only listens on 127.0.0.1.

The mock's tests check every response it sends against `openapi/v1.yml`. A coverage test makes sure every operation, and every status code the contract documents, is exercised.

## License

Apache 2.0. See [`LICENSE`](LICENSE).
