//! Append-only session storage. The log is authoritative; memory is a replayed view.
use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, Context, Result};
use goose_provider_types::conversation::{
    message::Message, token_usage::ProviderUsage, Conversation,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const FORMAT_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct SessionMetadata {
    pub cwd: PathBuf,
    pub provider: String,
    pub model: String,
}

impl SessionMetadata {
    fn validate(&self) -> Result<()> {
        if !self.cwd.is_absolute() {
            bail!("session working directory must be absolute");
        }
        if self.provider.trim().is_empty() || self.model.trim().is_empty() {
            bail!("session provider and model must not be empty");
        }
        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct Session {
    pub id: String,
    pub created_at: u64,
    pub metadata: SessionMetadata,
    pub conversation: Conversation,
    pub usage: Vec<ProviderUsage>,
}

/// Metadata changes and message deltas are events, never edits to earlier lines.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Record {
    Created {
        version: u32,
        id: String,
        created_at: u64,
        metadata: SessionMetadata,
    },
    Metadata {
        metadata: SessionMetadata,
    },
    Message {
        message: Message,
    },
    Usage {
        usage: ProviderUsage,
    },
}

impl Session {
    fn apply(&mut self, record: Record) {
        match record {
            Record::Metadata { metadata } => self.metadata = metadata,
            // Preserve GDK delta merging, including metadata-only messages.
            Record::Message { message } => self.conversation.push(message),
            Record::Usage { usage } => self.usage.push(usage),
            Record::Created { .. } => unreachable!("creation is only valid as the first record"),
        }
    }
}

pub(crate) struct SessionStore {
    // Holding the file also holds its exclusive advisory lock until drop.
    file: File,
    pub path: PathBuf,
    pub session: Session,
    failed: bool,
}

impl Drop for SessionStore {
    fn drop(&mut self) {
        // Release explicitly: a concurrently spawned shell may briefly inherit the
        // descriptor before exec closes it, which would otherwise keep flock held.
        let _ = self.file.unlock();
    }
}

impl SessionStore {
    pub fn create(directory: &Path, metadata: SessionMetadata) -> Result<Self> {
        metadata.validate()?;
        fs::create_dir_all(directory)
            .with_context(|| format!("create session directory {}", directory.display()))?;
        let id = Uuid::new_v4().to_string();
        let path = directory.join(format!("{id}.jsonl"));
        let mut options = OpenOptions::new();
        options.read(true).append(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(&path)
            .with_context(|| format!("create session {}", path.display()))?;
        file.try_lock()
            .with_context(|| format!("lock session {}", path.display()))?;
        let created_at = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let record = Record::Created {
            version: FORMAT_VERSION,
            id: id.clone(),
            created_at,
            metadata: metadata.clone(),
        };
        let mut store = Self {
            file,
            path,
            session: Session {
                id,
                created_at,
                metadata,
                conversation: Conversation::empty(),
                usage: Vec::new(),
            },
            failed: false,
        };
        store.write_records(&[record])?;
        Ok(store)
    }

    pub fn open(path: &Path) -> Result<Self> {
        if path
            .extension()
            .is_none_or(|extension| extension != "jsonl")
        {
            bail!("session file must have a .jsonl extension");
        }
        // Never create or truncate when resuming, including on a bad path.
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(path)
            .with_context(|| format!("open session {}", path.display()))?;
        file.try_lock()
            .with_context(|| format!("lock session {} (is it already open?)", path.display()))?;
        let session = replay(&file).with_context(|| format!("read session {}", path.display()))?;
        Ok(Self {
            file,
            path: path.to_owned(),
            session,
            failed: false,
        })
    }

    pub fn update_metadata(&mut self, metadata: SessionMetadata) -> Result<()> {
        if metadata != self.session.metadata {
            self.append(vec![Record::Metadata { metadata }])?;
        }
        Ok(())
    }

    pub fn append(&mut self, records: Vec<Record>) -> Result<()> {
        for record in &records {
            validate_update(record)?;
        }
        self.write_records(&records)?;
        // Do not expose unpersisted state if a write or sync failed.
        for record in records {
            self.session.apply(record);
        }
        Ok(())
    }

    fn write_records(&mut self, records: &[Record]) -> Result<()> {
        if self.failed {
            bail!("session log previously failed to write; reopen it before continuing");
        }
        if records.is_empty() {
            return Ok(());
        }
        // Serialize before touching the file; every committed record ends in LF.
        let mut bytes = Vec::new();
        for record in records {
            serde_json::to_writer(&mut bytes, record)?;
            bytes.push(b'\n');
        }
        if let Err(error) = self
            .file
            .write_all(&bytes)
            .and_then(|()| self.file.sync_data())
        {
            self.failed = true;
            return Err(error).with_context(|| format!("append session {}", self.path.display()));
        }
        Ok(())
    }
}

fn validate_update(record: &Record) -> Result<()> {
    match record {
        Record::Created { .. } => {
            bail!("session creation must be the first and only creation record")
        }
        Record::Metadata { metadata } => metadata.validate(),
        _ => Ok(()),
    }
}

fn replay(file: &File) -> Result<Session> {
    let mut reader = BufReader::new(file);
    let mut session: Option<Session> = None;
    let mut bytes = Vec::new();
    let mut line = 0;
    loop {
        bytes.clear();
        if reader.read_until(b'\n', &mut bytes)? == 0 {
            break;
        }
        line += 1;
        // Fail closed on a torn final append. Never truncate or silently discard it.
        if bytes.last() != Some(&b'\n') {
            bail!("incomplete record at line {line}; session log was not modified");
        }
        let record: Record = serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid session record at line {line}"))?;
        if let Some(session) = session.as_mut() {
            validate_update(&record).with_context(|| format!("invalid record at line {line}"))?;
            session.apply(record);
        } else {
            let Record::Created {
                version,
                id,
                created_at,
                metadata,
            } = record
            else {
                bail!("first session record must be creation metadata");
            };
            if version != FORMAT_VERSION {
                bail!("unsupported session format version {version}");
            }
            if id.is_empty() {
                bail!("session ID must not be empty");
            }
            metadata.validate()?;
            session = Some(Session {
                id,
                created_at,
                metadata,
                conversation: Conversation::empty(),
                usage: Vec::new(),
            });
        }
    }
    session.context("empty session log")
}

#[cfg(test)]
mod tests;
