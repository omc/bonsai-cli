<p align="center">
  <a href="https://bonsai.io"><img src="docs/assets/bonsai-logo.png" alt="Bonsai" width="320"></a>
</p>

<h1 align="center">Bonsai CLI</h1>

<p align="center">
  The command-line client for <a href="https://bonsai.io">Bonsai</a>, managed Elasticsearch and OpenSearch.
</p>

<p align="center">
  <a href="https://bonsai.io/docs/cli/overview/"><b>Documentation</b></a>
  &nbsp;&nbsp;&nbsp;
  <a href="https://bonsai.io/docs/cli/quickstart/"><b>Quickstart</b></a>
  &nbsp;&nbsp;&nbsp;
  <a href="https://bonsai.io/docs/agents/overview/"><b>AI agents</b></a>
  &nbsp;&nbsp;&nbsp;
  <a href="https://bonsai.io/docs/cli/reference/"><b>Command reference</b></a>
  &nbsp;&nbsp;&nbsp;
  <a href="CHANGELOG.md"><b>Changelog</b></a>
</p>

<p align="center">
  <a href="https://github.com/omc/bonsai-cli/releases/latest/download/bonsai-aarch64-apple-darwin.tar.xz"><img src="https://img.shields.io/badge/macOS-Apple%20silicon-1f2328?style=for-the-badge&logo=apple&logoColor=white" alt="Download for macOS, Apple silicon"></a>
  <a href="https://github.com/omc/bonsai-cli/releases/latest/download/bonsai-x86_64-apple-darwin.tar.xz"><img src="https://img.shields.io/badge/macOS-Intel-1f2328?style=for-the-badge&logo=apple&logoColor=white" alt="Download for macOS, Intel"></a>
  <a href="https://github.com/omc/bonsai-cli/releases/latest/download/bonsai-x86_64-unknown-linux-musl.tar.xz"><img src="https://img.shields.io/badge/Linux-x86--64-1f2328?style=for-the-badge&logo=linux&logoColor=white" alt="Download for Linux, x86-64"></a>
  <a href="https://github.com/omc/bonsai-cli/releases/latest/download/bonsai-aarch64-unknown-linux-musl.tar.xz"><img src="https://img.shields.io/badge/Linux-ARM64-1f2328?style=for-the-badge&logo=linux&logoColor=white" alt="Download for Linux, ARM64"></a>
</p>

`bonsai` creates, resizes and destroys clusters, gets their connection URLs, and shows their usage, metrics and top search queries, from your terminal, from CI, or through a coding agent. It talks to the [Bonsai API](https://bonsai.io/docs/api/overview/) with your account's token, so anything it does, the dashboard and the API can do too.

```sh
bonsai login
bonsai cluster create search --plan standard-fir --region aws-us-east-1
bonsai cluster url search --write .env
```

<p align="center">
  <img src="docs/assets/demo.gif" alt="A terminal session: creating a cluster, writing its connection URL to .env, then checking its usage and listing clusters" width="800">
</p>

## Install

On macOS or Linux, the install script downloads the binary for your machine:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://bonsai.io/install.sh | sh
```

To install by hand, download the archive for your machine with one of the buttons above, or from the [latest release](https://github.com/omc/bonsai-cli/releases/latest), and put `bonsai` on your `PATH`. Each release lists its checksums in `sha256.sum`. There are builds for macOS (Apple silicon and Intel) and Linux (x86-64 and ARM64). There is no Windows build.

With Nix:

```sh
nix profile install github:omc/bonsai-cli
```

From source, with Rust 1.88 or newer:

```sh
git clone https://github.com/omc/bonsai-cli
cd bonsai-cli
cargo install --path crates/bonsai
```

If you used the install script, `bonsai-update` moves you to the latest release. See [Install and log in](https://bonsai.io/docs/cli/install/).

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

The [quickstart](https://bonsai.io/docs/cli/quickstart/) walks through the same steps with their output.

## Commands

| Area | Commands |
|---|---|
| Account | `login`, `logout`, `whoami`, `doctor`, `version` |
| Clusters | `cluster list`, `cluster create`, `cluster show`, `cluster open`, `cluster rename`, `cluster resize`, `cluster destroy`, `cluster wait`, `cluster history` |
| Connecting | `cluster url` |
| Data | `cluster request` |
| Watching | `cluster usage`, `cluster metrics`, `cluster queries` |
| Actions | `action show`, `wait` |
| Catalogue | `plans`, `regions`, `spaces`, `releases` |
| Anything else | `api`, `completion` |
| Agents | `skill install`, `skill print`, `mcp` |

`bonsai <command> --help` shows each command's flags, and the [command reference](https://bonsai.io/docs/cli/reference/) lists them all.

A cluster can be named by its ID or by its name. Set `BONSAI_CLUSTER` to give commands a default cluster.

Slow work such as creating, resizing and destroying runs as an Action on Bonsai's side. The CLI waits for it by default. With `--no-wait` it returns at once, and `bonsai wait <action-id>` picks the wait up again later. Ctrl-C stops waiting, not the Action.

`bonsai api` sends any request to the API with your token, for things that have no command yet:

```sh
bonsai api GET /clusters --paginate
bonsai api POST /clusters/<cluster-id>/credentials -d '{"name": "reader", "access_type": "ro"}'
```

It prints the API's response as it is, so it can print passwords. Paths take cluster IDs, not names.

`bonsai cluster request` sends a request to the cluster's own API, Elasticsearch's or OpenSearch's, with the cluster's credential. It's how you create indices, load documents and search without handling the password:

```sh
bonsai cluster request search PUT /products -d @mapping.json
bonsai cluster request search POST /products/_bulk -d @products.ndjson
bonsai cluster request search POST /products/_search -d '{"query": {"match": {"name": "fern"}}}'
bonsai cluster request search GET '/_cat/indices?v'
bonsai cluster request search DELETE /products --confirm products
```

It prints the cluster's answer as it came. When the cluster answers with an error, the exit code is 3 for a 401 or 403, 4 for a 404, and 1 otherwise. Deleting an index, or running `_delete_by_query`, asks you to type the index's name; without a terminal it needs `--confirm`.

## Scripting and CI

Every command takes `--json`, which prints the API's JSON for scripts and `jq`. Messages and progress go to stderr and data goes to stdout, so `$(bonsai cluster url search)` captures only the URL.

Commands never prompt when stdin isn't a terminal, when `CI` is set, or with `--no-input`. Instead they fail with exit code 2 and name the missing flag.

```sh
id=$(bonsai cluster create "ci-$RUN_ID" --plan sandbox --region aws-us-east-1 --json | jq -r .result.cluster)
export BONSAI_URL=$(bonsai cluster url "$id")
run-tests
bonsai cluster destroy "$id" --confirm "ci-$RUN_ID"
```

Exit codes say what went wrong: 3 for a missing token or scope, 4 for not found, 5 for a failed Action, 9 for a timeout. [Scripting and CI](https://bonsai.io/docs/cli/scripting/) has the full list.

## AI agents

A coding agent can manage clusters with the same binary, in two ways. You can use both.

`bonsai skill install` writes a [Claude Code skill](https://docs.claude.com/en/docs/claude-code/skills) to `~/.claude/skills/bonsai/SKILL.md`. It tells the agent to use `--json --no-input`, which commands to run, what the exit codes mean, and to confirm before changes that cost money. For other agents, `bonsai skill print` prints the same text, e.g. to add to `AGENTS.md`. It also teaches `bonsai cluster request`, so an agent can create indices, load documents and search without seeing the cluster's password.

`bonsai mcp` is an [MCP](https://modelcontextprotocol.io) server on stdin and stdout, for clients that call tools instead of running a shell. To add it to Claude Code, run this in your project folder:

```sh
claude mcp add bonsai -- bonsai mcp
```

Other clients (Claude Desktop, Cursor, VS Code) take the usual `mcpServers` entry: `{"bonsai": {"command": "bonsai", "args": ["mcp"]}}`.

The agent never sees your API token or a cluster's password. The server uses the token `bonsai login` stored, so nothing goes in the client's config, and `write_cluster_url` writes the connection URL to a git-ignored env file and answers with the password as `****`. `destroy_cluster` needs the cluster's current name as confirmation, and `bonsai mcp --read-only` leaves out the tools that change clusters.

See [Using Bonsai with AI agents](https://bonsai.io/docs/agents/overview/) for setup by client, example prompts and the safety model.

## Documentation

| | |
|---|---|
| [Overview](https://bonsai.io/docs/cli/overview/) | What the CLI does, and when to use the API or Terraform instead |
| [Install and log in](https://bonsai.io/docs/cli/install/) | Installing, logging in, where the token is kept, updating |
| [Quickstart](https://bonsai.io/docs/cli/quickstart/) | Create a cluster, connect to it, watch it and destroy it |
| [Managing clusters](https://bonsai.io/docs/cli/clusters/) | Create, show, rename, resize and destroy, and how Actions run |
| [Connection URLs](https://bonsai.io/docs/cli/connection-urls/) | Get a cluster's URL without leaking its password |
| [Usage, metrics and queries](https://bonsai.io/docs/cli/monitoring/) | What a cluster holds and how it's used |
| [Scripting and CI](https://bonsai.io/docs/cli/scripting/) | JSON output, exit codes, running without prompts, `bonsai api` |
| [Agent skill](https://bonsai.io/docs/cli/agent-skill/) | Teach Claude Code, or another agent, to use the CLI |
| [MCP server](https://bonsai.io/docs/cli/mcp-server/) | The tools `bonsai mcp` serves, and how to connect a client |
| [Configuration](https://bonsai.io/docs/cli/configuration/) | Profiles, environment variables, files, shell completion, update checks |
| [Command reference](https://bonsai.io/docs/cli/reference/) | Every command and flag |

## Support

If you have a question, a problem, or something you'd like the CLI to do, write to [support@bonsai.io](mailto:support@bonsai.io).

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
- `--local-clusters` makes the mock answer a small part of each cluster's own API too (indices, documents, `_bulk`, `_search`), and points credential URLs at it, so `bonsai cluster request` works. Without it, a cluster's URL names a host that doesn't exist.
- `--public-url` sets the origin the mock puts in links and `Location` headers, and accepts its host as a `Host`; use it behind a TLS proxy.
- `--checkout-url` makes payment `action_url`s that prefix plus a fake Stripe session ID, instead of the mock's own checkout page.
- `bonsai-mock token create --account acme --scopes "…"` makes more tokens.

The mock only listens on 127.0.0.1.

The mock's tests check every response it sends against `openapi/v1.yml`. A coverage test makes sure every operation, and every status code the contract documents, is exercised.

## License

Apache 2.0. See [`LICENSE`](LICENSE).
