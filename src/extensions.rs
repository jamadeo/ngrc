//! Explicit, launch-scoped MCP configuration; the GDK owns transports and dispatch.
use std::{path::Path, sync::Arc};

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use goose_agent::{
    action_required_manager::ActionRequiredManager,
    extension::{name_to_key, ExtensionConfig},
    mcp::{
        ClientContext, ExtensionHost, ExtensionLease, ExtensionManager, ExtensionSet,
        ExtensionSource,
    },
};

use crate::Session;

struct Host;
impl ExtensionHost<ExtensionConfig> for Host {}

pub(crate) struct Extensions {
    configs: Vec<ExtensionConfig>,
    manager: Arc<ExtensionManager<ExtensionConfig>>,
}

impl Extensions {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let configs: Vec<ExtensionConfig> = match path {
            Some(path) => serde_json::from_slice(
                &std::fs::read(path)
                    .with_context(|| format!("read extensions {}", path.display()))?,
            )
            .with_context(|| {
                format!(
                    "parse extensions {} (expected a JSON array)",
                    path.display()
                )
            })?,
            None => Vec::new(),
        };
        for config in &configs {
            if name_to_key(&config.name()).is_empty() {
                bail!("extension name must have a nonempty key");
            }
            match config {
                ExtensionConfig::Stdio { cmd, cwd, .. } => {
                    if cmd.trim().is_empty() {
                        bail!("extension '{}' has an empty command", config.name());
                    }
                    if cwd
                        .as_ref()
                        .is_some_and(|cwd| !Path::new(cwd).is_absolute())
                    {
                        bail!("extension '{}' cwd must be absolute", config.name());
                    }
                }
                ExtensionConfig::StreamableHttp { uri, .. } if uri.trim().is_empty() => {
                    bail!("extension '{}' has an empty URI", config.name());
                }
                _ => {}
            }
        }
        // Validate normalized-name collisions before starting any process.
        ExtensionSet::new("validation", None, configs.clone())?;
        Ok(Self {
            configs,
            manager: Arc::new(ExtensionManager::new(
                Arc::new(Host),
                Arc::new(ActionRequiredManager::new()),
                ClientContext::new("ngrc", env!("CARGO_PKG_VERSION")),
            )),
        })
    }

    pub async fn start(&self, session: &Session) -> Result<()> {
        let lease = self.advertise(session).await?;
        for result in lease.start().await {
            if !result.success {
                bail!(
                    "start extension '{}': {}",
                    result.name,
                    result.error.unwrap_or_default()
                );
            }
            eprintln!("Extension: {}", result.name);
        }
        Ok(())
    }

    pub async fn release(&self, session_id: &str) {
        self.manager.release(session_id).await;
    }
}

#[async_trait]
impl ExtensionSource<Session, ExtensionConfig> for Extensions {
    async fn advertise(&self, session: &Session) -> Result<ExtensionLease<ExtensionConfig>> {
        let set = ExtensionSet::new(
            session.id.clone(),
            Some(session.metadata.cwd.clone()),
            self.configs.clone(),
        )?;
        Ok(self.manager.resolve(&set).await)
    }
}
