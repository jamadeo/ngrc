//! OS capabilities, not model-facing tools. Plugins decide how to expose these.
use std::{
    fs::File,
    io::Read,
    path::{Component, Path, PathBuf},
    process::Stdio,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExecInput {
    program: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default = "default_timeout", rename = "timeoutMs")]
    timeout_ms: u64,
}

fn default_timeout() -> u64 {
    120_000
}

#[derive(Serialize)]
struct ExecOutput {
    stdout: String,
    stderr: String,
    exit_code: Option<i32>,
}

#[derive(Clone)]
pub(crate) struct HostState {
    pub cwd: PathBuf,
    pub cancel: CancellationToken,
    pub runtime: tokio::runtime::Handle,
    allow_exec: bool,
    reads: Arc<AtomicUsize>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ReadFileInput {
    path: String,
    #[serde(default = "default_read_bytes")]
    max_bytes: usize,
}

fn default_read_bytes() -> usize {
    16 * 1024
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ListDirInput {
    #[serde(default = "default_directory")]
    path: String,
    #[serde(default = "default_entries")]
    max_entries: usize,
}

fn default_directory() -> String {
    ".".into()
}
fn default_entries() -> usize {
    100
}

impl HostState {
    pub fn new(cwd: PathBuf, cancel: CancellationToken, allow_exec: bool) -> Self {
        Self {
            cwd,
            cancel,
            runtime: tokio::runtime::Handle::current(),
            allow_exec,
            reads: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn read_path(&self, path: &str) -> Result<PathBuf> {
        if self.cancel.is_cancelled() {
            bail!("plugin invocation cancelled");
        }
        if self.reads.fetch_add(1, Ordering::Relaxed) >= 64 {
            bail!("filesystem read budget exceeded (64 calls per invocation)");
        }
        let path = Path::new(path);
        if path.is_absolute()
            || path
                .components()
                .any(|part| matches!(part, Component::ParentDir))
        {
            bail!("filesystem paths must be relative to cwd without '..'");
        }
        let root = self.cwd.canonicalize()?;
        let resolved = root.join(path);
        // Reject symlinks, including intermediate components, rather than following
        // project pointers to files elsewhere. This is not a race-proof sandbox.
        let mut prefix = root.clone();
        for component in path.components() {
            prefix.push(component);
            match std::fs::symlink_metadata(&prefix) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    bail!("filesystem reads do not follow symlinks")
                }
                Ok(_) => (),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(error) => return Err(error.into()),
            }
        }
        Ok(resolved)
    }

    pub fn read_file(&self, input: ReadFileInput) -> Result<serde_json::Value> {
        if !(1..=64 * 1024).contains(&input.max_bytes) {
            bail!("maxBytes must be between 1 and 65536");
        }
        let path = self.read_path(&input.path)?;
        let metadata = match std::fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(serde_json::Value::Null)
            }
            Err(error) => return Err(error.into()),
        };
        if !metadata.is_file() {
            bail!("host.readFile requires a regular file");
        }
        let mut bytes = Vec::new();
        File::open(path)?
            .take((input.max_bytes + 1) as u64)
            .read_to_end(&mut bytes)?;
        let truncated = bytes.len() > input.max_bytes;
        bytes.truncate(input.max_bytes);
        Ok(serde_json::json!({ "text": String::from_utf8_lossy(&bytes), "truncated": truncated }))
    }

    pub fn list_dir(&self, input: ListDirInput) -> Result<serde_json::Value> {
        if !(1..=200).contains(&input.max_entries) {
            bail!("maxEntries must be between 1 and 200");
        }
        let path = self.read_path(&input.path)?;
        // Bound scanning and allocation even for unexpectedly large directories.
        let mut entries = std::fs::read_dir(path)?
            .take(4097)
            .collect::<std::io::Result<Vec<_>>>()?;
        let scan_truncated = entries.len() > 4096;
        entries.truncate(4096);
        entries.sort_by_key(|entry| entry.file_name());
        let truncated = scan_truncated || entries.len() > input.max_entries;
        let entries = entries.into_iter().take(input.max_entries).map(|entry| {
            let kind = entry.file_type()?;
            Ok(serde_json::json!({ "name": entry.file_name().to_string_lossy(),
                "kind": if kind.is_dir() { "directory" } else if kind.is_file() { "file" } else { "other" } }))
        }).collect::<Result<Vec<_>>>()?;
        Ok(serde_json::json!({ "entries": entries, "truncated": truncated }))
    }

    // Called on the dedicated Boa thread, never on a Tokio executor thread.
    pub fn exec(&self, input: ExecInput) -> Result<serde_json::Value> {
        if !self.allow_exec {
            bail!("host.exec is only available during tool execution, not context()");
        }
        if input.program.is_empty() || !(1..=120_000).contains(&input.timeout_ms) {
            bail!("host.exec requires a program and timeoutMs between 1 and 120000");
        }
        self.runtime.block_on(async {
            if self.cancel.is_cancelled() {
                bail!("tool call cancelled");
            }
            eprintln!("\nexec: {} {:?}", input.program, input.args);
            let mut command = tokio::process::Command::new(input.program);
            command
                .args(input.args)
                .current_dir(&self.cwd)
                .stdin(Stdio::null())
                .kill_on_drop(true);
            let output = tokio::select! {
                biased;
                _ = self.cancel.cancelled() => bail!("tool call cancelled"),
                output = tokio::time::timeout(Duration::from_millis(input.timeout_ms), command.output()) => {
                    output.context("process timed out")?.context("execute process")?
                }
            };
            Ok(serde_json::to_value(ExecOutput {
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                exit_code: output.status.code(),
            })?)
        })
    }
}
