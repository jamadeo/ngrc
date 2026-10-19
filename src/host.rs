//! OS capabilities, not model-facing tools. Plugins decide how to expose these.
use std::{path::PathBuf, process::Stdio, time::Duration};

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
}

impl HostState {
    // Called on the dedicated Boa thread, never on a Tokio executor thread.
    pub fn exec(&self, input: ExecInput) -> Result<serde_json::Value> {
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
