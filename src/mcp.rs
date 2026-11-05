//! Application-owned JSON config; protocol and tool routing belong to the GDK.
use std::{collections::BTreeMap, path::Path, sync::Arc, time::Duration};

use anyhow::{bail, Context, Result};
use goose_agent::mcp::{
    ClientOptions, HttpConnector, Lease, Registration, StartupPolicy, StderrPolicy, StdioConnector,
};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(rename = "mcpServers")]
    servers: BTreeMap<String, Server>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
enum Server {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
    },
    Http {
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
}

fn read_config(path: &Path) -> Result<Config> {
    let bytes =
        std::fs::read(path).with_context(|| format!("read MCP config {}", path.display()))?;
    let config: Config = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse MCP config {}", path.display()))?;
    for name in config.servers.keys() {
        if name.trim().is_empty() {
            bail!("MCP server name must not be empty");
        }
    }
    Ok(config)
}

/// Fail startup rather than silently run with missing configured tools.
/// Connections and their pinned catalog are retained for the entire chat.
pub(crate) async fn connect(
    path: &Path,
    cwd: &Path,
    scope: &str,
    cancel: CancellationToken,
) -> Result<Arc<Lease>> {
    let config = read_config(path)?;
    let mut registrations = Vec::new();
    for (name, server) in config.servers {
        let options = ClientOptions {
            timeout: Duration::from_secs(30),
            ..Default::default()
        };
        let registration = match server {
            Server::Stdio { command, args, env } => {
                let mut connector = StdioConnector::new(command, options)
                    .with_context(|| format!("MCP server {name}"))?;
                connector.args = args;
                connector.env = env;
                connector.inherit_env = true;
                connector.cwd = Some(cwd.to_path_buf());
                connector.stderr = StderrPolicy::Inherit;
                Registration::stdio(name, connector)
            }
            Server::Http { url, headers } => Registration::http(
                name.clone(),
                HttpConnector::new(&url, headers, options)
                    .with_context(|| format!("MCP server {name}"))?,
            ),
        };
        registrations.push(registration);
    }
    Ok(Arc::new(
        Lease::connect(scope, registrations, StartupPolicy::Strict, cancel).await?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_transports_and_rejects_unknown_fields() {
        let config: Config = serde_json::from_str(r#"{"mcpServers": {
            "local": {"type":"stdio", "command":"server", "args":["--flag"], "env":{"KEY":"value"}},
            "remote": {"type":"http", "url":"https://example.com/mcp", "headers":{"Authorization":"Bearer token"}}
        }}"#).unwrap();
        assert_eq!(config.servers.len(), 2);
        for invalid in [
            r#"{"mcpServers":{"x":{"type":"sse","url":"https://example.com"}}}"#,
            r#"{"mcpServers":{"x":{"type":"stdio"}}}"#,
            r#"{"mcpServers":{"x":{"type":"http","url":"http://localhost","command":"oops"}}}"#,
            r#"{"servers":{}}"#,
        ] {
            assert!(
                serde_json::from_str::<Config>(invalid).is_err(),
                "{invalid}"
            );
        }
    }
}
