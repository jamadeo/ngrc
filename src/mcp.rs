//! Application-owned JSON config; protocol and tool routing belong to the GDK.
use std::{collections::BTreeMap, path::Path, process::Stdio, sync::Arc, time::Duration};

use anyhow::{bail, Context, Result};
use goose_agent::mcp::{ClientOptions, Lease, Manager, Registration, TransportClient};
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
    for (name, server) in &config.servers {
        if name.trim().is_empty() {
            bail!("MCP server name must not be empty");
        }
        match server {
            Server::Stdio { command, .. } if command.trim().is_empty() => {
                bail!("MCP server {name}: command must not be empty")
            }
            Server::Http { url, .. } => {
                let url = reqwest::Url::parse(url)
                    .with_context(|| format!("MCP server {name}: invalid URL"))?;
                if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
                    bail!("MCP server {name}: URL must use http or https");
                }
            }
            _ => {}
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
        let connection = async {
            match server {
                Server::Stdio { command, args, env } => {
                    let mut process = tokio::process::Command::new(command);
                    process
                        .args(args)
                        .envs(env)
                        .current_dir(cwd)
                        .stderr(Stdio::inherit())
                        .kill_on_drop(true);
                    TransportClient::stdio(process, options).await
                }
                Server::Http { url, headers } => {
                    let mut defaults = reqwest::header::HeaderMap::new();
                    for (key, value) in headers {
                        defaults.insert(
                            reqwest::header::HeaderName::from_bytes(key.as_bytes())?,
                            reqwest::header::HeaderValue::from_str(&value)?,
                        );
                    }
                    let client = reqwest::Client::builder()
                        .default_headers(defaults)
                        .build()?;
                    TransportClient::http(client,
                        rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(url),
                        options).await
                }
            }
        };
        let client = tokio::select! {
            biased;
            _ = cancel.cancelled() => bail!("MCP connection cancelled"),
            result = connection => result.with_context(|| format!("connect MCP server {name}"))?,
        };
        registrations.push(Registration::attached(name, 0, Arc::new(client)));
    }
    let lease = Arc::new(Manager::new().resolve(scope, registrations).await?);
    tokio::select! {
        biased;
        _ = cancel.cancelled() => bail!("MCP discovery cancelled"),
        result = lease.catalog(cancel.clone()) => { result.context("discover MCP tools")?; }
    }
    Ok(lease)
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
