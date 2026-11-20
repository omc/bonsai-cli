//! `bonsai mcp`: an MCP server on stdin and stdout, for Claude Code and other
//! clients.
//!
//! The tools are `bonsai-mcp-tools`', the same ones the hosted server offers. This
//! file is only what is this server's own: it runs with the one token `bonsai
//! login` stored, in the caller's project, for as long as the client keeps it. So
//! it can write a cluster's URL to a git-ignored env file (`write_cluster_url`),
//! it waits longer than a server behind a router can, and `--read-only` leaves
//! out the tools that change a cluster and keeps `cluster_request` to reading.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use bonsai_api::Client;
use bonsai_core::{CoreError, connection_url, resolve_cluster};
use bonsai_mcp_tools::{Asked, Host, Limits, Tools, UrlTarget};
use rmcp::ServiceExt;
use rmcp::model::Extensions;
use serde_json::Value;

use super::{Ctx, ca_bundle, env_file, mcp_user_agent};
use crate::cli::McpArgs;
use crate::exit::Interrupted;

pub async fn run(ctx: &Ctx, args: McpArgs) -> Result<()> {
    let a = ctx.authed_with(mcp_user_agent()).await?;
    let host = Local {
        client: Arc::new(a.client),
        root: std::env::current_dir()?,
        read_only: args.read_only,
    };
    tracing::debug!(read_only = args.read_only, "serving MCP on stdio");
    let service = Tools::new(host).serve(rmcp::transport::stdio()).await?;
    tokio::select! {
        res = service.waiting() => { res?; Ok(()) }
        () = ctx.cancel.cancelled() => Err(Interrupted.into()),
    }
}

/// This server, to the tools.
struct Local {
    client: Arc<Client>,
    /// The folder the client started the server in: `write_cluster_url` writes only
    /// below it.
    root: PathBuf,
    /// `--read-only`.
    read_only: bool,
}

impl Host for Local {
    fn server_info(&self) -> (&'static str, &'static str) {
        ("bonsai", env!("CARGO_PKG_VERSION"))
    }

    fn instructions(&self) -> String {
        "Manage Bonsai (managed Elasticsearch and OpenSearch) clusters. \
         A cluster argument is an ID or a name; a name two clusters share is refused, use the ID. \
         create_cluster, resize_cluster and destroy_cluster return an Action at once: call \
         wait_action with its id until its state is success or error. No tool returns a \
         cluster's password: write_cluster_url writes the connection URL to a git-ignored env \
         file for the app to read, and cluster_request reads and writes the cluster's data \
         (indices, documents, searches) for you."
            .to_owned()
    }

    fn client(&self, _extensions: &Extensions) -> Result<Arc<Client>> {
        Ok(self.client.clone())
    }

    /// Nothing in front of this server drops a slow call, so nothing is budgeted.
    fn limits(&self) -> Limits {
        Limits {
            wait_default: Duration::from_secs(60),
            wait_max: Duration::from_secs(300),
            wait_budget: None,
            request_default: Duration::from_secs(60),
            request_max: Duration::from_secs(300),
            request_budget: None,
            body_max: None,
        }
    }

    fn user_agent(&self) -> String {
        mcp_user_agent()
    }

    fn ca_bundle(&self) -> Option<PathBuf> {
        ca_bundle()
    }

    fn read_only(&self) -> bool {
        self.read_only
    }

    fn missing_scope(&self, scope: Option<&str>) -> String {
        match scope {
            Some(scope) => {
                format!("this token lacks {scope}; create a token with it in the dashboard")
            }
            None => "this token lacks the scope this needs; create a token with it in the \
                     dashboard"
                .to_owned(),
        }
    }

    fn wrong_name(&self, name: &str) -> String {
        format!("confirm must be the cluster's current name, {name:?}; nothing was destroyed")
    }

    fn request_needs(&self) -> &'static str {
        "Needs the credentials:read scope."
    }

    /// `--read-only` sends only what reads. Otherwise the token's scopes decide,
    /// when the credential is read.
    async fn permit(&self, _client: &Client, asked: &Asked<'_>) -> Result<()> {
        if self.read_only && !asked.reads {
            return Err(CoreError::Usage(format!(
                "{} {} isn't sent under --read-only, which sends only GET and HEAD; nothing \
                 was sent",
                asked.method, asked.path
            ))
            .into());
        }
        Ok(())
    }

    fn has_project(&self) -> bool {
        true
    }

    async fn write_url(&self, client: &Client, target: UrlTarget) -> Result<Value> {
        let variable = target.variable.as_deref().unwrap_or(env_file::DEFAULT_VAR);
        if !super::url::is_env_name(variable) {
            return Err(CoreError::Usage(format!(
                "{variable:?} isn't a variable name (letters, digits and _, not starting with a digit)"
            ))
            .into());
        }
        let path = target.path.as_deref().unwrap_or(env_file::DEFAULT_FILE);
        // Checked before the URL is fetched, so a refusal never holds the secret.
        let file = env_file::Target::check(&self.root, path)?;
        let cluster = resolve_cluster(client, &target.cluster).await?.value;
        let url = connection_url(client, &cluster.id, target.credential.as_deref()).await?;
        let root = self.root.clone();
        let var = variable.to_owned();
        let written = tokio::task::spawn_blocking(move || file.write(&root, &var, &url)).await??;
        Ok(serde_json::json!({
            "path": written.path,
            "variable": variable,
            "url": written.redacted,
            "replaced": written.replaced,
        }))
    }
}
