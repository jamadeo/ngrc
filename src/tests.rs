use super::*;
use goose_agent::{operation::Operation, tool::ToolProvider};
use goose_provider_types::{
    base::{stream_from_single_message, MessageStream},
    errors::ProviderError,
};
use rmcp::model::{CallToolRequestParams, Tool};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

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

fn runtime() -> Runtime {
    Runtime(Mutex::new(Session {
        conversation: Conversation::new_unvalidated([Message::user().with_text("test")]),
        cwd: std::env::temp_dir(),
    }))
}

async fn shell_plugins() -> Result<Arc<Plugins>> {
    Plugins::from_sources(vec![plugins::PluginSource {
        name: "shell.js".into(),
        source: include_str!("../plugins/shell.js").into(),
    }])
    .await
}

fn emitter(cancel: CancellationToken) -> Emitter {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    Emitter::new(tx, cancel)
}

async fn call_plugin(
    plugins: &Plugins,
    name: &str,
    arguments: Value,
    cancel: CancellationToken,
) -> Result<Value> {
    let result = plugins
        .call(
            &runtime().load("main").await?,
            "test-call",
            CallToolRequestParams::new(name.to_owned())
                .with_arguments(arguments.as_object().unwrap().clone()),
            &emitter(cancel),
        )
        .await?;
    Ok(result.structured_content.expect("JSON result"))
}

#[tokio::test]
async fn only_plugin_shell_is_registered_and_captures_output() -> Result<()> {
    let session = runtime().load("main").await?;
    let plugins = shell_plugins().await?;
    let operation = ToolOperation::<Session>::new().with_provider(plugins.clone());
    let tools = <ToolOperation<Session> as Operation<Session, Effect>>::inference_tools(
        &operation, &session,
    )
    .await?;
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name.as_ref(), "shell");
    assert!(tools[0].input_schema["properties"]["command"].is_object());
    // stdin must immediately reach EOF, not wait for user input.
    let output = call_plugin(
        &plugins,
        "shell",
        json!({
            "command": "cat; printf hello; printf err >&2; exit 7"
        }),
        CancellationToken::new(),
    )
    .await?;
    assert_eq!(output["stdout"], "hello");
    assert_eq!(output["stderr"], "err");
    assert_eq!(output["exit_code"], 7);
    let output = call_plugin(
        &plugins,
        "shell",
        json!({"command": "pwd -P"}),
        CancellationToken::new(),
    )
    .await?;
    assert_eq!(
        PathBuf::from(output["stdout"].as_str().unwrap().trim()),
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
    let machine = agent(
        provider.clone(),
        ModelConfig::new("test"),
        cancel,
        shell_plugins().await?,
    );
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
    let session = agent(
        Arc::new(FailingProvider),
        ModelConfig::new("test"),
        cancel,
        shell_plugins().await?,
    )
    .run(&runtime(), "main", &emitter)
    .await?;
    assert!(session.conversation.last().unwrap().error_kind().is_some());
    Ok(())
}

#[tokio::test]
async fn shell_plugin_uses_bash() -> Result<()> {
    let plugins = shell_plugins().await?;
    let output = call_plugin(&plugins, "shell", json!({
        "command": "[[ -n $BASH_VERSION ]] && values=(hello bash) && printf '%s' \"${values[1]}\""
    }), CancellationToken::new()).await?;
    assert_eq!(output["exit_code"], 0);
    assert_eq!(output["stdout"], "bash");
    Ok(())
}

fn empty_runtime() -> Runtime {
    Runtime(Mutex::new(Session {
        conversation: Conversation::empty(),
        cwd: std::env::temp_dir(),
    }))
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
                shell_plugins().await?,
                provider.clone(),
                ModelConfig::new("test"),
                initial,
                rx,
                CancellationToken::new(),
            ),
        )
        .await??;
        let session = runtime.load("main").await?;
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
        shell_plugins().await?,
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
    assert!(runtime.load("main").await?.conversation.is_empty());
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
        shell_plugins().await?,
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
        shell_plugins().await?,
        Arc::new(FailingProvider),
        ModelConfig::new("test"),
        Some("first".into()),
        rx,
        CancellationToken::new(),
    )
    .await?;
    let session = runtime.load("main").await?;
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

mod context_tests;
mod plugin_tests;
