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
            n => panic!("unexpected provider call {n}"),
        };
        Ok(stream_from_single_message(
            message,
            ProviderUsage::new("test".into(), Default::default()),
        ))
    }
}

fn runtime() -> Runtime {
    Runtime(Mutex::new(Session {
        conversation: Conversation::new_unvalidated([Message::user().with_text("test")]),
        cwd: std::env::temp_dir(),
    }))
}

#[tokio::test]
async fn only_shell_is_registered_and_captures_output() -> Result<()> {
    let session = runtime().load("main").await?;
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
        session.cwd.canonicalize()?
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
    let session = tokio::time::timeout(
        Duration::from_secs(5),
        machine.run(&runtime(), "main", &emitter),
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
    let session = agent(Arc::new(FailingProvider), ModelConfig::new("test"), cancel)
        .run(&runtime(), "main", &emitter)
        .await?;
    assert!(session.conversation.last().unwrap().error_kind().is_some());
    Ok(())
}
