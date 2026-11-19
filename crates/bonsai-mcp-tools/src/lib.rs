//! Bonsai's MCP tools, once, for every server that offers them.
//!
//! Two servers do: `bonsai mcp`, which runs on a person's machine and talks on
//! stdin and stdout, and the hosted one at `mcp.bonsai.io`, which answers over
//! HTTP. They used to carry a copy of the tools each. Here a tool is written one
//! time, and a server says only what is its own through [`Host`]: where the API
//! token comes from, how long a call may take, and what it does that the other
//! can't.
//!
//! Each tool returns the API's JSON as it came. A tool that starts an Action
//! returns the Action at once, and `wait_action` follows it. No tool returns a
//! cluster's password.
//!
//! To add a tool, add it in [`tools`]: both servers get it with their next build.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use bonsai_api::{Client, EnginePath};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::model::{Extensions, Tool};
use serde_json::Value;

mod request;
mod text;
mod tools;

pub use text::{json_string, sanitize};

/// The tools that change a cluster. A server asked to only read
/// ([`Host::read_only`]) doesn't list them.
pub const CHANGING_TOOLS: [&str; 4] = [
    "create_cluster",
    "rename_cluster",
    "resize_cluster",
    "destroy_cluster",
];

/// The tool that writes a cluster's connection URL to a file in the caller's
/// project. Listed only by a server that has such a folder
/// ([`Host::has_project`]).
pub const WRITE_URL_TOOL: &str = "write_cluster_url";

/// How long the calls that wait may take, and how much they may carry.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// What `wait_action` waits by default, and at most.
    pub wait_default: Duration,
    pub wait_max: Duration,
    /// All one `wait_action` call may take, the reads before and after the wait
    /// included, and how much of that is kept back for the read after. `None`
    /// where nothing in front of the server drops a slow request.
    pub wait_budget: Option<(Duration, Duration)>,
    /// What `cluster_request` gives the cluster to answer by default, and at most.
    pub request_default: Duration,
    pub request_max: Duration,
    /// All one `cluster_request` call may take, reading the credential included.
    pub request_budget: Option<Duration>,
    /// The largest body `cluster_request` sends.
    pub body_max: Option<usize>,
}

/// One `cluster_request`, as a server is asked whether it may be sent.
#[derive(Debug)]
pub struct Asked<'a> {
    pub method: &'a http::Method,
    pub path: &'a EnginePath,
    /// Whether the request only reads: a GET or a HEAD that removes nothing.
    pub reads: bool,
}

/// What `write_cluster_url` was asked to write, and where.
#[derive(Debug)]
pub struct UrlTarget {
    /// The cluster's ID or name.
    pub cluster: String,
    pub path: Option<String>,
    pub variable: Option<String>,
    pub credential: Option<String>,
}

/// What a server brings to the tools: everything that isn't the same for the two.
///
/// The methods with a default are the ones only one server has an answer for.
pub trait Host: Send + Sync + 'static {
    /// The server's name and version, for the MCP handshake.
    fn server_info(&self) -> (&'static str, &'static str);

    /// What the server tells a client about itself: how the tools fit together.
    fn instructions(&self) -> String;

    /// An API client holding the token this call runs with. A server with one
    /// token ignores `extensions`; one that takes a token with each request
    /// reads it from the HTTP request there.
    fn client(&self, extensions: &Extensions) -> Result<Arc<Client>>;

    fn limits(&self) -> Limits;

    /// The `User-Agent` requests to a cluster carry.
    fn user_agent(&self) -> String;

    /// A PEM file of extra root certificates, for requests to a cluster.
    fn ca_bundle(&self) -> Option<PathBuf> {
        None
    }

    /// Whether to leave out the tools that change a cluster.
    fn read_only(&self) -> bool {
        false
    }

    /// What to tell a person whose token lacks `scope` (`None` when the API didn't
    /// name it): how they get a token that has it differs by server.
    fn missing_scope(&self, scope: Option<&str>) -> String;

    /// What `destroy_cluster` answers when `confirm` isn't the cluster's name.
    fn wrong_name(&self, name: &str) -> String;

    /// The last sentence of `cluster_request`'s description: what the token needs.
    fn request_needs(&self) -> &'static str;

    /// Whether this caller may send `asked` to a cluster. Called before any
    /// credential is read. An error is a refusal; say in it that nothing was sent.
    fn permit(&self, client: &Client, asked: &Asked<'_>)
    -> impl Future<Output = Result<()>> + Send;

    /// Whether the server runs in the caller's project, and so can write a
    /// cluster's URL to a file there.
    fn has_project(&self) -> bool {
        false
    }

    /// Writes a cluster's connection URL where `target` says, and returns what
    /// was written with the password left out.
    fn write_url(
        &self,
        _client: &Client,
        _target: UrlTarget,
    ) -> impl Future<Output = Result<Value>> + Send {
        async {
            Err(anyhow::anyhow!(
                "this server has no project folder to write to"
            ))
        }
    }
}

/// The MCP server: the tools, for one [`Host`].
pub struct Tools<H: Host> {
    host: Arc<H>,
    tool_router: ToolRouter<Self>,
}

impl<H: Host> Clone for Tools<H> {
    fn clone(&self) -> Self {
        Self {
            host: self.host.clone(),
            tool_router: self.tool_router.clone(),
        }
    }
}

impl<H: Host> Tools<H> {
    pub fn new(host: H) -> Self {
        let host = Arc::new(host);
        let mut tool_router = ToolRouter::new();
        for mut route in Self::tool_router() {
            let name = route.attr.name.as_ref();
            let left_out = (host.read_only() && CHANGING_TOOLS.contains(&name))
                || (!host.has_project() && name == WRITE_URL_TOOL);
            if left_out {
                continue;
            }
            describe(&mut route.attr, host.as_ref());
            tool_router.add_route(route);
        }
        Self { host, tool_router }
    }

    pub fn host(&self) -> &H {
        &self.host
    }
}

/// Finishes the descriptions that state a server's own limits.
fn describe<H: Host>(tool: &mut Tool, host: &H) {
    let limits = host.limits();
    let more = match tool.name.as_ref() {
        "wait_action" => format!(
            " Waits {} seconds by default, and {} at most.",
            limits.wait_default.as_secs(),
            limits.wait_max.as_secs()
        ),
        "cluster_request" => {
            let mut more = format!(
                " The cluster has {} seconds to answer by default, and {} at most. The answer \
                 is at most {} KiB.",
                limits.request_default.as_secs(),
                limits.request_max.as_secs(),
                request::ANSWER_MAX >> 10
            );
            if let Some(max) = limits.body_max {
                more.push_str(&format!(" The body is at most {} MiB.", max >> 20));
            }
            if host.read_only() {
                more.push_str(" This server is read-only: it sends only GET and HEAD.");
            }
            more.push(' ');
            more.push_str(host.request_needs());
            more
        }
        _ => return,
    };
    let described = format!("{}{more}", tool.description.as_deref().unwrap_or_default());
    tool.description = Some(described.into());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A server with nothing behind it: enough to build the tool list.
    struct Fake {
        read_only: bool,
        project: bool,
        body_max: Option<usize>,
    }

    impl Host for Fake {
        fn server_info(&self) -> (&'static str, &'static str) {
            ("fake", "0")
        }
        fn instructions(&self) -> String {
            String::new()
        }
        fn client(&self, _extensions: &Extensions) -> Result<Arc<Client>> {
            Err(anyhow::anyhow!("no API"))
        }
        fn limits(&self) -> Limits {
            Limits {
                wait_default: Duration::from_secs(60),
                wait_max: Duration::from_secs(300),
                wait_budget: None,
                request_default: Duration::from_secs(15),
                request_max: Duration::from_secs(20),
                request_budget: None,
                body_max: self.body_max,
            }
        }
        fn user_agent(&self) -> String {
            "fake".to_owned()
        }
        fn read_only(&self) -> bool {
            self.read_only
        }
        fn missing_scope(&self, _scope: Option<&str>) -> String {
            String::new()
        }
        fn wrong_name(&self, _name: &str) -> String {
            String::new()
        }
        fn request_needs(&self) -> &'static str {
            "Needs a scope."
        }
        async fn permit(&self, _client: &Client, _asked: &Asked<'_>) -> Result<()> {
            Ok(())
        }
        fn has_project(&self) -> bool {
            self.project
        }
    }

    fn tools(read_only: bool, project: bool, body_max: Option<usize>) -> Vec<Tool> {
        let fake = Fake {
            read_only,
            project,
            body_max,
        };
        Tools::new(fake).tool_router.list_all()
    }

    fn names(tools: &[Tool]) -> Vec<&str> {
        tools.iter().map(|t| t.name.as_ref()).collect()
    }

    fn description<'a>(tools: &'a [Tool], name: &str) -> &'a str {
        let tool = tools.iter().find(|t| t.name == name).unwrap();
        tool.description.as_deref().unwrap()
    }

    #[test]
    fn a_server_lists_every_tool_it_can_have() {
        let all = tools(false, true, None);
        assert_eq!(all.len(), 19, "{:?}", names(&all));
        for tool in CHANGING_TOOLS
            .iter()
            .chain([&WRITE_URL_TOOL, &"cluster_request"])
        {
            assert!(names(&all).contains(tool), "{tool}");
        }
    }

    #[test]
    fn a_read_only_server_leaves_out_the_tools_that_change_a_cluster() {
        let read_only = tools(true, true, None);
        assert_eq!(read_only.len(), 15);
        for tool in CHANGING_TOOLS {
            assert!(!names(&read_only).contains(&tool), "{tool}");
        }
        // Reading a cluster's data and writing a local file stay.
        assert!(names(&read_only).contains(&"cluster_request"));
        assert!(names(&read_only).contains(&WRITE_URL_TOOL));
        assert!(description(&read_only, "cluster_request").contains("read-only"));
    }

    #[test]
    fn a_server_without_a_project_has_no_tool_that_writes_a_file() {
        let hosted = tools(false, false, None);
        assert_eq!(hosted.len(), 18);
        assert!(!names(&hosted).contains(&WRITE_URL_TOOL));
    }

    #[test]
    fn descriptions_state_the_servers_own_limits() {
        let capped = tools(false, false, Some(1 << 20));
        assert!(
            description(&capped, "wait_action")
                .ends_with("Waits 60 seconds by default, and 300 at most.")
        );
        let request = description(&capped, "cluster_request");
        for said in [
            "15 seconds to answer by default, and 20 at most",
            "The answer is at most 256 KiB.",
            "The body is at most 1 MiB.",
        ] {
            assert!(request.contains(said), "{said}: {request}");
        }
        assert!(request.ends_with("Needs a scope."), "{request}");
        assert!(!request.contains("read-only"), "{request}");

        let uncapped = tools(false, false, None);
        assert!(!description(&uncapped, "cluster_request").contains("The body is at most"));
        // A tool with no limit of the server's is described as written.
        assert_eq!(
            description(&capped, "whoami"),
            description(&uncapped, "whoami")
        );
    }
}
