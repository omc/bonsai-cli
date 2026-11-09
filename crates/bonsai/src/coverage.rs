//! Operation coverage: every `operationId` in `openapi/v1.yml` and the commands that call it.
//! Test-only: the table exists so a new operation fails CI until it
//! is mapped to a command or marked as reachable only through `bonsai api`.

/// `(operationId, commands)`. Each command is a space-separated subcommand path as typed
/// after `bonsai`. An empty slice means reachable only through `bonsai api`, on purpose.
/// `mcp` lists the operations its tools call.
pub const COVERAGE: &[(&str, &[&str])] = &[
    // `login` shows the API Tokens page before asking for a token.
    ("getApiRoot", &["doctor", "version", "login"]),
    ("getAccount", &["login", "whoami", "doctor", "mcp"]),
    // `login` reads a pasted token's expiry.
    ("getToken", &["login", "whoami"]),
    ("revokeToken", &["logout"]),
    // Every command that takes a CLUSTER resolves it: getCluster by ID, then
    // listClusters by name (bonsai-core's resolve_cluster).
    (
        "listClusters",
        &[
            "clusters",
            "cluster list",
            "cluster show",
            "cluster open",
            "cluster rename",
            "cluster resize",
            "cluster destroy",
            "cluster history",
            "history",
            "cluster wait",
            "url",
            "usage",
            "metrics",
            "queries",
            "mcp",
        ],
    ),
    ("createCluster", &["cluster create", "mcp"]),
    (
        "getCluster",
        &[
            "cluster show",
            "cluster open",
            "cluster rename",
            "cluster resize",
            "cluster destroy",
            "cluster history",
            "history",
            "cluster wait",
            "cluster create",
            "url",
            "usage",
            "metrics",
            "queries",
            "mcp",
        ],
    ),
    ("updateCluster", &["cluster rename", "mcp"]),
    (
        "deleteCluster",
        &["cluster destroy", "cluster create", "mcp"],
    ),
    ("changeClusterPlan", &["cluster resize", "mcp"]),
    ("listClusterActions", &["cluster history", "history", "mcp"]),
    (
        "getAction",
        &[
            "action show",
            "wait",
            "cluster wait",
            "cluster create",
            "cluster resize",
            "cluster destroy",
            "mcp",
        ],
    ),
    ("getClusterUsage", &["usage", "mcp"]),
    ("getClusterMetrics", &["metrics", "mcp"]),
    ("getClusterTopQueries", &["queries", "mcp"]),
    ("listClusterCredentials", &["url", "mcp"]),
    ("createClusterCredential", &[]),
    ("getClusterCredential", &["url", "mcp"]),
    ("revokeClusterCredential", &[]),
    ("listPlans", &["regions", "plans", "cluster create", "mcp"]),
    // `cluster create --plan` asks only among the plan's regions.
    ("getPlan", &["cluster resize", "cluster create"]),
    ("listRegions", &["regions", "cluster create", "mcp"]),
    ("listSpaces", &["spaces", "cluster create", "mcp"]),
    ("listReleases", &["releases", "cluster create", "mcp"]),
    ("startCliLogin", &["login"]),
    ("pollCliLogin", &["login"]),
];

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use bonsai_contract::Contract;
    use clap::{Command, CommandFactory};

    use super::COVERAGE;
    use crate::cli::Cli;

    #[test]
    fn coverage_matches_contract_operations() {
        let contract = Contract::load_default();
        let in_contract: BTreeSet<&str> = contract
            .operations()
            .iter()
            .map(|op| op.id.as_str())
            .collect();
        let in_table: BTreeSet<&str> = COVERAGE.iter().map(|(id, _)| *id).collect();
        assert_eq!(
            COVERAGE.len(),
            in_table.len(),
            "duplicate operationId in COVERAGE"
        );
        let missing: Vec<_> = in_contract.difference(&in_table).collect();
        let stale: Vec<_> = in_table.difference(&in_contract).collect();
        assert!(
            missing.is_empty() && stale.is_empty(),
            "map new operations to a command (or `&[]` for api-only); \
             missing from COVERAGE: {missing:?}; not in contract: {stale:?}"
        );
    }

    #[test]
    fn api_only_operations_are_the_credential_writes() {
        let api_only: BTreeSet<&str> = COVERAGE
            .iter()
            .filter(|(_, cmds)| cmds.is_empty())
            .map(|(id, _)| *id)
            .collect();
        assert_eq!(
            api_only,
            BTreeSet::from(["createClusterCredential", "revokeClusterCredential"])
        );
    }

    fn resolves(root: &Command, path: &str) -> bool {
        let mut cur = root;
        for word in path.split_whitespace() {
            match cur.find_subcommand(word) {
                Some(next) => cur = next,
                None => return false,
            }
        }
        true
    }

    /// Every command path in the tree that takes a `CLUSTER` argument.
    fn cluster_taking(cmd: &Command, prefix: &str, out: &mut Vec<String>) {
        for sub in cmd.get_subcommands() {
            let path = format!("{prefix}{}", sub.get_name()).trim().to_owned();
            let takes = sub.get_arguments().any(|a| {
                a.get_value_names()
                    .is_some_and(|names| names.iter().any(|n| n == "CLUSTER"))
            });
            if takes {
                out.push(path.clone());
            }
            cluster_taking(sub, &format!("{path} "), out);
        }
    }

    /// A `CLUSTER` argument goes through `resolve_cluster`: `getCluster`, then
    /// `listClusters` when the argument isn't an ID.
    #[test]
    fn every_cluster_taking_command_lists_the_lookup() {
        let mut commands = Vec::new();
        cluster_taking(&Cli::command(), "", &mut commands);
        assert!(commands.len() >= 10, "{commands:?}");
        let of = |id: &str| -> &[&str] {
            COVERAGE
                .iter()
                .find(|(op, _)| *op == id)
                .map(|(_, cmds)| *cmds)
                .unwrap()
        };
        for op in ["getCluster", "listClusters"] {
            let missing: Vec<_> = commands
                .iter()
                .filter(|c| !of(op).contains(&c.as_str()))
                .collect();
            assert!(missing.is_empty(), "{op} doesn't list {missing:?}");
        }
    }

    #[test]
    fn every_named_command_exists_in_the_clap_tree() {
        let root = Cli::command();
        let unknown: Vec<_> = COVERAGE
            .iter()
            .flat_map(|(id, cmds)| cmds.iter().map(move |c| (*id, *c)))
            .filter(|(_, c)| !resolves(&root, c))
            .collect();
        assert!(unknown.is_empty(), "not in the clap tree: {unknown:?}");
    }
}
