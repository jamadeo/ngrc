use super::*;
use goose_provider_types::conversation::token_usage::Usage;

struct Directory(PathBuf);

impl Directory {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!("ngrc-store-test-{}", Uuid::new_v4())))
    }

    fn create(&self) -> Result<SessionStore> {
        SessionStore::create(&self.0, metadata())
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn metadata() -> SessionMetadata {
    SessionMetadata {
        cwd: std::env::temp_dir(),
        provider: "test-provider".into(),
        model: "test-model".into(),
    }
}

#[test]
fn messages_usage_and_metadata_round_trip_without_rewriting() -> Result<()> {
    let directory = Directory::new();
    let mut store = directory.create()?;
    let id = store.session.id.clone();
    let created_at = store.session.created_at;
    let path = store.path.clone();
    assert_eq!(path.extension().unwrap(), "jsonl");
    let header = fs::read(&path)?;
    let message = Message::user()
        .with_text("a line\nand a second line: 🦆")
        .with_generated_id_if_missing();
    store.append(vec![Record::Message {
        message: message.clone(),
    }])?;
    let before_metadata = fs::read(&path)?;
    assert!(before_metadata.starts_with(&header));
    let mut updated = metadata();
    updated.model = "another-model".into();
    store.update_metadata(updated.clone())?;
    let after_metadata = fs::read(&path)?;
    assert!(after_metadata.starts_with(&before_metadata));
    store.update_metadata(updated.clone())?;
    assert_eq!(fs::read(&path)?, after_metadata); // unchanged metadata is a no-op
    let usage = ProviderUsage::new("another-model".into(), Usage::new(Some(10), Some(2), None))
        .with_response_id("response-1".into());
    store.append(vec![Record::Usage {
        usage: usage.clone(),
    }])?;
    let final_bytes = fs::read(&path)?;
    assert!(final_bytes.starts_with(&after_metadata));
    assert_eq!(final_bytes.iter().filter(|&&byte| byte == b'\n').count(), 4);
    drop(store);

    let store = SessionStore::open(&path)?;
    assert_eq!(store.session.id, id);
    assert_eq!(store.session.created_at, created_at);
    assert_eq!(store.session.metadata, updated);
    assert_eq!(store.session.conversation.messages(), &[message]);
    assert_eq!(
        serde_json::to_value(&store.session.usage)?,
        serde_json::to_value(vec![usage])?
    );
    assert_eq!(fs::read(&path)?, final_bytes); // opening must not append or rewrite
    Ok(())
}

#[test]
fn replay_merges_stream_deltas_and_metadata_only_messages() -> Result<()> {
    let directory = Directory::new();
    let mut store = directory.create()?;
    let path = store.path.clone();
    let first = Message::assistant()
        .with_text("hello ")
        .with_generated_id_if_missing();
    let mut delta = Message::assistant().with_text("world");
    delta.id = first.id.clone();
    let mut metadata_only = Message::assistant();
    metadata_only.id = first.id.clone();
    metadata_only.metadata.output_token_limit_reached = true;
    for message in [first, delta, metadata_only] {
        let before = fs::read(&path)?;
        store.append(vec![Record::Message { message }])?;
        assert!(fs::read(&path)?.starts_with(&before));
    }
    let expected = store.session.conversation.clone();
    assert_eq!(expected.len(), 1);
    assert_eq!(expected.last().unwrap().as_concat_text(), "hello world");
    assert!(expected.last().unwrap().metadata.output_token_limit_reached);
    drop(store);
    assert_eq!(SessionStore::open(&path)?.session.conversation, expected);
    Ok(())
}

#[test]
fn locks_prevent_concurrent_writers_and_release_on_drop() -> Result<()> {
    let directory = Directory::new();
    let store = directory.create()?;
    let path = store.path.clone();
    assert!(SessionStore::open(&path).is_err());
    drop(store);
    let _reopened = SessionStore::open(&path)?;
    Ok(())
}

#[test]
fn each_new_session_gets_a_separate_file() -> Result<()> {
    let directory = Directory::new();
    let first = directory.create()?;
    let second = directory.create()?;
    assert_ne!(first.session.id, second.session.id);
    assert_ne!(first.path, second.path);
    assert_eq!(fs::read_dir(&directory.0)?.count(), 2);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&first.path)?.permissions().mode() & 0o777,
            0o600
        );
    }
    Ok(())
}

#[test]
fn corrupt_and_incomplete_logs_are_rejected_without_modification() -> Result<()> {
    let directory = Directory::new();
    let store = directory.create()?;
    let path = store.path.clone();
    let header = fs::read(&path)?;
    drop(store);
    for tail in [
        b"not json\n".as_slice(),
        b"{\"type\":",
        b"\n",
        header.as_slice(),
    ] {
        let mut bytes = header.clone();
        bytes.extend_from_slice(tail);
        fs::write(&path, &bytes)?;
        assert!(SessionStore::open(&path).is_err());
        assert_eq!(fs::read(&path)?, bytes);
    }
    // Even a syntactically valid JSON record without its final LF is incomplete.
    let bytes = header[..header.len() - 1].to_vec();
    fs::write(&path, &bytes)?;
    assert!(SessionStore::open(&path).is_err());
    assert_eq!(fs::read(&path)?, bytes);
    Ok(())
}

#[test]
fn validates_header_format_and_missing_paths() -> Result<()> {
    let directory = Directory::new();
    let store = directory.create()?;
    let path = store.path.clone();
    let mut header: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
    drop(store);
    header["version"] = 999.into();
    let unsupported = format!("{header}\n");
    for bytes in [
        "",
        "{\"type\":\"future\"}\n",
        &unsupported,
        "{\"type\":\"metadata\",\"metadata\":{}}\n",
    ] {
        fs::write(&path, bytes)?;
        assert!(SessionStore::open(&path).is_err());
        assert_eq!(fs::read(&path)?, bytes.as_bytes());
    }
    let missing = directory.0.join("missing.jsonl");
    assert!(SessionStore::open(&missing).is_err());
    assert!(!missing.exists());
    assert!(SessionStore::open(&directory.0.join("wrong.txt")).is_err());
    Ok(())
}

#[test]
fn invalid_metadata_and_duplicate_creation_do_not_append() -> Result<()> {
    let directory = Directory::new();
    let mut store = directory.create()?;
    let bytes = fs::read(&store.path)?;
    let mut invalid = metadata();
    invalid.cwd = PathBuf::from("relative");
    assert!(store.update_metadata(invalid).is_err());
    let mut invalid = metadata();
    invalid.model.clear();
    assert!(store.update_metadata(invalid).is_err());
    assert!(store
        .append(vec![Record::Created {
            version: FORMAT_VERSION,
            id: "duplicate".into(),
            created_at: 0,
            metadata: metadata(),
        }])
        .is_err());
    assert_eq!(fs::read(&store.path)?, bytes);
    assert_eq!(store.session.metadata, metadata());
    Ok(())
}

#[cfg(unix)]
#[test]
fn failed_writes_do_not_change_memory_or_allow_further_appends() -> Result<()> {
    let directory = Directory::new();
    let mut store = directory.create()?;
    // A read-only handle deterministically fails write_all, without permissions races.
    store.file = File::open(&store.path)?;
    assert!(store
        .append(vec![Record::Message {
            message: Message::user().with_text("must not be visible"),
        }])
        .is_err());
    assert!(store.session.conversation.is_empty());
    assert!(store.failed);
    assert!(store
        .append(vec![Record::Message {
            message: Message::user().with_text("still must not be visible"),
        }])
        .is_err());
    Ok(())
}
