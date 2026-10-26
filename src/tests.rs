use super::*;
use goose_agent::operation::Operation;
use goose_provider_types::{
    base::{stream_from_single_message, MessageStream},
    errors::ProviderError,
};
use rmcp::model::{CallToolRequestParams, Tool};
use std::sync::atomic::{AtomicUsize, Ordering};

struct ScriptedProvider(AtomicUsize);

#[async_trait]
impl Provider for ScriptedProvider {
    fn get_name(&self) -> &str {
        "scripted"
    }

    async fn stream(
        &self,
        _: &ModelConfig,
        system: &str,
        messages: &[Message],
        tools: &[Tool],
    ) -> Result<MessageStream, ProviderError> {
        assert!(system.contains("coding agent"));
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name.as_ref(), "shell");
        let message = match self.0.fetch_add(1, Ordering::SeqCst) {
            0 => Message::assistant().with_tool_request(
                "shell-1",
                Ok(CallToolRequestParams::new("shell").with_arguments(
                    [("command".into(), "printf gdk-shell-ok".into())]
                        .into_iter()
                        .collect(),
                )),
            ),
            1 => {
                let response = messages
                    .iter()
                    .find(|message| message.get_tool_response_ids().contains(&"shell-1"))
                    .expect("shell response must be returned to the model");
                assert!(format!("{response:?}").contains("gdk-shell-ok"));
                Message::assistant().with_text("done")
            }
            2 => {
                let text: Vec<_> = messages.iter().map(Message::as_concat_text).collect();
                assert!(text.iter().any(|text| text == "test"));
                assert!(text.iter().any(|text| text == "done"));
                assert_eq!(text.last().unwrap(), "follow up");
                let response = messages
                    .iter()
                    .find(|message| message.get_tool_response_ids().contains(&"shell-1"))
                    .expect("previous shell result must remain in context");
                assert!(format!("{response:?}").contains("gdk-shell-ok"));
                Message::assistant().with_text("follow-up done")
            }
            n => panic!("unexpected provider call {n}"),
        };
        Ok(stream_from_single_message(
            message,
            ProviderUsage::new("test".into(), Default::default()),
        ))
    }
}

// Keep each test's on-disk log isolated and remove it after closing the store.
struct TestRuntime {
    runtime: Option<Runtime>,
    directory: PathBuf,
}

impl std::ops::Deref for TestRuntime {
    type Target = Runtime;

    fn deref(&self) -> &Runtime {
        self.runtime.as_ref().unwrap()
    }
}

impl Drop for TestRuntime {
    fn drop(&mut self) {
        drop(self.runtime.take());
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn runtime() -> TestRuntime {
    let runtime = empty_runtime();
    runtime
        .append_message(Message::user().with_text("test"))
        .unwrap();
    runtime
}

#[tokio::test]
async fn only_shell_is_registered_and_captures_output() -> Result<()> {
    let session = runtime().snapshot();
    let operation = ToolOperation::<Session>::new().with_async_tool::<Shell>();
    let tools = <ToolOperation<Session> as Operation<Session, Effect>>::inference_tools(
        &operation, &session,
    )
    .await?;
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name.as_ref(), "shell");
    assert!(tools[0].input_schema["properties"]["command"].is_object());
    let output = Shell::invoke(
        &session,
        ShellInput {
            // Reading stdin must immediately reach EOF, not wait for user input.
            command: "cat; printf hello; printf err >&2; exit 7".into(),
        },
    )
    .await?;
    assert_eq!(output.stdout, "hello");
    assert_eq!(output.stderr, "err");
    assert_eq!(output.exit_code, Some(7));
    let output = Shell::invoke(
        &session,
        ShellInput {
            command: "pwd -P".into(),
        },
    )
    .await?;
    assert_eq!(
        PathBuf::from(output.stdout.trim()),
        session.metadata.cwd.canonicalize()?
    );
    Ok(())
}

#[tokio::test]
async fn model_shell_model_loop_completes() -> Result<()> {
    let provider = Arc::new(ScriptedProvider(AtomicUsize::new(0)));
    let cancel = CancellationToken::new();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let emitter = Emitter::new(tx, cancel.clone());
    let machine = agent(provider.clone(), ModelConfig::new("test"), cancel);
    let runtime = runtime();
    let session = tokio::time::timeout(
        Duration::from_secs(5),
        machine.run(&*runtime, &runtime.snapshot().id, &emitter),
    )
    .await??;
    assert_eq!(
        session.conversation.last().unwrap().as_concat_text(),
        "done"
    );
    assert_eq!(provider.0.load(Ordering::SeqCst), 2);
    Ok(())
}

struct FailingProvider;

#[async_trait]
impl Provider for FailingProvider {
    fn get_name(&self) -> &str {
        "failing"
    }

    async fn stream(
        &self,
        _: &ModelConfig,
        _: &str,
        _: &[Message],
        _: &[Tool],
    ) -> Result<MessageStream, ProviderError> {
        Err(ProviderError::Authentication("test invalid key".into()))
    }
}

#[tokio::test]
async fn provider_errors_are_detectable_in_the_final_session() -> Result<()> {
    let cancel = CancellationToken::new();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let emitter = Emitter::new(tx, cancel.clone());
    let runtime = runtime();
    let session = agent(Arc::new(FailingProvider), ModelConfig::new("test"), cancel)
        .run(&*runtime, &runtime.snapshot().id, &emitter)
        .await?;
    assert!(session.conversation.last().unwrap().error_kind().is_some());
    Ok(())
}

#[tokio::test]
async fn shell_uses_bash() -> Result<()> {
    let output = Shell::invoke(
        &runtime().snapshot(),
        ShellInput {
            command:
                "[[ -n $BASH_VERSION ]] && values=(hello bash) && printf '%s' \"${values[1]}\""
                    .into(),
        },
    )
    .await?;
    assert_eq!(output.exit_code, Some(0));
    assert_eq!(output.stdout, "bash");
    Ok(())
}

fn empty_runtime() -> TestRuntime {
    let directory = std::env::temp_dir().join(format!("ngrc-test-{}", uuid::Uuid::new_v4()));
    let store = SessionStore::create(
        &directory,
        SessionMetadata {
            cwd: std::env::temp_dir(),
            provider: "scripted".into(),
            model: "test".into(),
        },
    )
    .unwrap();
    TestRuntime {
        runtime: Some(Runtime(Mutex::new(store))),
        directory,
    }
}

#[tokio::test]
async fn chat_keeps_context_with_or_without_initial_arguments() -> Result<()> {
    for initial in [None, Some("test".to_owned())] {
        let runtime = empty_runtime();
        let provider = Arc::new(ScriptedProvider(AtomicUsize::new(0)));
        let (tx, rx) = tokio::sync::mpsc::channel(5);
        tx.send(Ok("   ".into())).await?;
        if initial.is_none() {
            tx.send(Ok("test".into())).await?;
        }
        tx.send(Ok("follow up".into())).await?;
        drop(tx); // EOF after the queued lines.
        tokio::time::timeout(
            Duration::from_secs(5),
            chat(
                &runtime,
                provider.clone(),
                ModelConfig::new("test"),
                initial,
                rx,
                CancellationToken::new(),
            ),
        )
        .await??;
        let session = runtime.snapshot();
        assert_eq!(
            session.conversation.last().unwrap().as_concat_text(),
            "follow-up done"
        );
        assert_eq!(provider.0.load(Ordering::SeqCst), 3);
        let user_prompts: Vec<_> = session
            .conversation
            .iter()
            .filter(|message| {
                message.role == rmcp::model::Role::User && !message.is_tool_response()
            })
            .map(Message::as_concat_text)
            .collect();
        assert_eq!(user_prompts, ["test", "follow up"]);
    }
    Ok(())
}

#[tokio::test]
async fn chat_waits_without_an_initial_message_and_exits_on_eof() -> Result<()> {
    let runtime = empty_runtime();
    let provider = Arc::new(ScriptedProvider(AtomicUsize::new(0)));
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let task_runtime = &runtime;
    let task_provider = provider.clone();
    let chat = chat(
        task_runtime,
        task_provider,
        ModelConfig::new("test"),
        None,
        rx,
        CancellationToken::new(),
    );
    tokio::pin!(chat);
    tokio::select! {
        result = &mut chat => panic!("chat exited before EOF: {result:?}"),
        _ = tokio::time::sleep(Duration::from_millis(25)) => {}
    }
    assert_eq!(provider.0.load(Ordering::SeqCst), 0);
    tx.send(Ok(" \t".into())).await?;
    drop(tx);
    tokio::time::timeout(Duration::from_secs(1), chat).await??;
    assert!(runtime.snapshot().conversation.is_empty());
    assert_eq!(provider.0.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn idle_chat_can_be_cancelled() -> Result<()> {
    let runtime = empty_runtime();
    let provider = Arc::new(ScriptedProvider(AtomicUsize::new(0)));
    let (_tx, rx) = tokio::sync::mpsc::channel(1);
    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    let chat = chat(
        &runtime,
        provider.clone(),
        ModelConfig::new("test"),
        None,
        rx,
        cancel,
    );
    let cancel_later = async {
        tokio::time::sleep(Duration::from_millis(25)).await;
        stop.cancel();
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(1), async {
        tokio::join!(chat, cancel_later)
    })
    .await?;
    result?;
    assert_eq!(provider.0.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn provider_failure_does_not_close_chat() -> Result<()> {
    let runtime = empty_runtime();
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    tx.send(Ok("retry".into())).await?;
    drop(tx);
    chat(
        &runtime,
        Arc::new(FailingProvider),
        ModelConfig::new("test"),
        Some("first".into()),
        rx,
        CancellationToken::new(),
    )
    .await?;
    let session = runtime.snapshot();
    assert_eq!(
        session
            .conversation
            .iter()
            .filter(|message| message.error_kind().is_some())
            .count(),
        2
    );
    assert_eq!(session.conversation.len(), 4);
    Ok(())
}

#[tokio::test]
async fn resumed_chat_restores_shell_results_and_appends_follow_up() -> Result<()> {
    let mut runtime = runtime();
    let provider = Arc::new(ScriptedProvider(AtomicUsize::new(0)));
    run_turn(
        &runtime,
        provider.clone(),
        ModelConfig::new("test"),
        CancellationToken::new(),
    )
    .await?;
    let path = runtime.0.lock().unwrap().path.clone();
    let before = std::fs::read(&path)?;
    let expected = runtime.snapshot();
    drop(runtime.runtime.take());

    let store = SessionStore::open(&path)?;
    assert_eq!(store.session.conversation, expected.conversation);
    assert_eq!(store.session.usage.len(), 2);
    runtime.runtime = Some(Runtime(Mutex::new(store)));
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    drop(tx);
    chat(
        &runtime,
        provider.clone(),
        ModelConfig::new("test"),
        Some("follow up".into()),
        rx,
        CancellationToken::new(),
    )
    .await?;
    assert!(std::fs::read(&path)?.starts_with(&before));
    let expected = runtime.snapshot();
    assert_eq!(expected.id, expected.id());
    assert_eq!(expected.usage.len(), 3);
    assert_eq!(
        expected.conversation.last().unwrap().as_concat_text(),
        "follow-up done"
    );
    drop(runtime.runtime.take());
    let reopened = SessionStore::open(&path)?;
    assert_eq!(reopened.session.conversation, expected.conversation);
    assert_eq!(reopened.session.usage.len(), 3);
    Ok(())
}

#[tokio::test]
async fn session_loader_rejects_other_session_ids() -> Result<()> {
    let runtime = runtime();
    let session = runtime.snapshot();
    assert_eq!(runtime.load(&session.id).await?.id, session.id);
    assert!(runtime.load("not-this-session").await.is_err());
    let mut other = session.clone();
    other.id = "another-session".into();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let emitter = Emitter::new(tx, CancellationToken::new());
    assert!(runtime
        .apply_effects(&other, &mut [], &emitter)
        .await
        .is_err());
    Ok(())
}

#[test]
fn parses_session_options_without_changing_prompt_arguments() -> Result<()> {
    fn parse(args: &[&str]) -> Result<Options> {
        Options::parse(args.iter().map(|arg| (*arg).to_owned()))
    }
    assert!(parse(&[])?.initial.is_none());
    assert!(parse(&["--help"])?.help);
    let options = parse(&["--session-dir", "a directory", "hello", "world"])?;
    assert_eq!(options.directory, Some(PathBuf::from("a directory")));
    assert_eq!(options.initial.as_deref(), Some("hello world"));
    let options = parse(&["--resume", "saved.jsonl", "--", "--help"])?;
    assert_eq!(options.resume, Some(PathBuf::from("saved.jsonl")));
    assert_eq!(options.initial.as_deref(), Some("--help"));
    // Everything after the first prompt word remains prompt text.
    assert_eq!(
        parse(&["hello", "--resume"])?.initial.as_deref(),
        Some("hello --resume")
    );
    for args in [
        vec!["--resume"],
        vec!["--session-dir"],
        vec!["--resume", ""],
        vec!["--unknown"],
        vec!["--resume", "a.jsonl", "--resume", "b.jsonl"],
        vec!["--session-dir", "a", "--session-dir", "b"],
        vec!["--session-dir", "a", "--resume", "b.jsonl"],
    ] {
        assert!(parse(&args).is_err(), "should reject {args:?}");
    }
    Ok(())
}
