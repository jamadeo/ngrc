use super::*;
use plugins::PluginSource;

fn source(name: &str, body: &str) -> PluginSource {
    PluginSource {
        name: name.into(),
        source: body.into(),
    }
}

fn tool_source(name: &str, body: &str) -> PluginSource {
    source(
        &format!("{name}.js"),
        &format!(
            r#"
        let count = 0;
        ({{ apiVersion: 1, tools: [{{
            name: {name:?}, description: "Test plugin",
            inputSchema: {{type: "object"}},
            execute(input, context) {{ {body} }}
        }}] }})
    "#
        ),
    )
}

#[tokio::test]
async fn arbitrary_tools_route_to_isolated_persistent_plugins() -> Result<()> {
    let plugins = Plugins::from_sources(vec![
        tool_source("alpha", "return {count: ++count, input, context};"),
        tool_source("beta", "return {count: ++count, input, context};"),
    ])
    .await?;
    let tools = plugins.tools(&runtime().load("main").await?).await?;
    assert_eq!(
        tools
            .iter()
            .map(|tool| tool.name.as_ref())
            .collect::<Vec<_>>(),
        ["alpha", "beta"]
    );
    for (name, expected) in [("alpha", 1), ("beta", 1), ("alpha", 2)] {
        let value = call_plugin(
            &plugins,
            name,
            json!({"text": "quotes \" and 雪"}),
            CancellationToken::new(),
        )
        .await?;
        assert_eq!(value["count"], expected);
        assert_eq!(value["input"]["text"], "quotes \" and 雪");
        assert_eq!(value["context"]["requestId"], "test-call");
        assert_eq!(
            value["context"]["cwd"],
            std::env::temp_dir().to_string_lossy().as_ref()
        );
    }
    let error = call_plugin(&plugins, "shell", json!({}), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("unknown plugin tool"));
    Ok(())
}

#[tokio::test]
async fn multiple_tools_can_share_one_plugin_context() -> Result<()> {
    let plugins = Plugins::from_sources(vec![source(
        "shared.js",
        r#"
        let count = 0;
        ({ apiVersion: 1, tools: ["one", "two"].map(name => ({
            name, description: "Increment", inputSchema: {type: "object"},
            execute() { return ++count; }
        })) })
    "#,
    )])
    .await?;
    assert_eq!(
        call_plugin(&plugins, "one", json!({}), CancellationToken::new()).await?,
        1
    );
    assert_eq!(
        call_plugin(&plugins, "two", json!({}), CancellationToken::new()).await?,
        2
    );
    Ok(())
}

#[tokio::test]
async fn invalid_plugins_fail_at_startup_with_filename() {
    for body in [
        "this is not valid javascript!",
        "42",
        "({apiVersion: 2, tools: []})",
        "({apiVersion: 1, tools: {}})",
        r#"({apiVersion: 1, tools: [{name: "no-handler", description: "x", inputSchema: {type: "object"}}]})"#,
        r#"({apiVersion: 1, tools: [{name: "bad name", description: "x", inputSchema: {type: "object"}, execute() {}}]})"#,
        r#"({apiVersion: 1, tools: [{name: "x", description: "x", inputSchema: {type: "array"}, execute() {}}]})"#,
        r#"({apiVersion: 1, tools: [{name: "x", description: "x", inputSchema: {type: "object", properties: {x: {type: "nonsense"}}}, execute() {}}]})"#,
        "host.exec({program: '/bin/echo'}); ({apiVersion: 1, tools: []})",
    ] {
        let error = Plugins::from_sources(vec![source("broken.js", body)])
            .await
            .err()
            .expect(body);
        assert!(error.to_string().contains("broken.js"), "{error:#}");
    }
}

#[tokio::test]
async fn duplicate_tool_names_are_rejected_within_and_across_files() {
    let duplicate = tool_source("same", "return null;");
    let error = Plugins::from_sources(vec![duplicate.clone(), duplicate])
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("duplicate tool name 'same'"));
    let error = Plugins::from_sources(vec![source(
        "duplicate.js",
        r#"
        const tool = {name: "same", description: "x", inputSchema: {type: "object"}, execute() {}};
        ({apiVersion: 1, tools: [tool, tool]})
    "#,
    )])
    .await
    .err()
    .unwrap();
    assert!(error.to_string().contains("duplicate tool name 'same'"));
}

#[tokio::test]
async fn schema_validation_happens_before_javascript_execution() -> Result<()> {
    let plugins = shell_plugins().await?;
    for input in [
        json!({}),
        json!({"command": 1}),
        json!({"command": "true", "extra": true}),
    ] {
        assert!(
            call_plugin(&plugins, "shell", input, CancellationToken::new())
                .await
                .is_err()
        );
    }
    Ok(())
}

#[tokio::test]
async fn execution_errors_are_recoverable_and_async_is_explicitly_rejected() -> Result<()> {
    for (body, expected) in [
        (
            "if (input.fail) throw new Error('plugin exploded'); return {ok: true};",
            "plugin exploded",
        ),
        (
            "if (input.fail) return undefined; return {ok: true};",
            "must return a JSON value",
        ),
        (
            "if (input.fail) return Promise.resolve(1); return {ok: true};",
            "async/Promise",
        ),
        (
            "if (input.fail) { const a = {}; a.a = a; return a; } return {ok: true};",
            "cyclic",
        ),
        (
            "if (input.fail) return host.exec({program: '/no/such/program'}); return {ok: true};",
            "execute process",
        ),
    ] {
        let plugins = Plugins::from_sources(vec![tool_source("test", body)]).await?;
        let error = call_plugin(
            &plugins,
            "test",
            json!({"fail": true}),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains(expected), "{error:#}");
        assert_eq!(
            call_plugin(&plugins, "test", json!({}), CancellationToken::new()).await?,
            json!({"ok": true})
        );
    }
    Ok(())
}

#[tokio::test]
async fn cancelled_calls_do_not_execute_javascript() -> Result<()> {
    let plugins = Plugins::from_sources(vec![tool_source("counter", "return ++count;")]).await?;
    let cancel = CancellationToken::new();
    cancel.cancel();
    let error = call_plugin(&plugins, "counter", json!({}), cancel)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("cancelled"));
    assert_eq!(
        call_plugin(&plugins, "counter", json!({}), CancellationToken::new()).await?,
        1
    );
    Ok(())
}

#[tokio::test]
async fn host_exec_rejects_invalid_options() -> Result<()> {
    let plugins =
        Plugins::from_sources(vec![tool_source("exec", "return host.exec(input);")]).await?;
    for input in [
        json!({}),
        json!({"program": ""}),
        json!({"program": "/bin/echo", "args": [3]}),
        json!({"program": "/bin/echo", "timeoutMs": 0}),
        json!({"program": "/bin/echo", "timeoutMs": 120001}),
        json!({"program": "/bin/echo", "cwd": "/"}),
    ] {
        assert!(
            call_plugin(&plugins, "exec", input, CancellationToken::new())
                .await
                .is_err()
        );
    }
    Ok(())
}

#[tokio::test]
async fn runaway_loop_is_bounded() -> Result<()> {
    let plugins = Plugins::from_sources(vec![tool_source("spin", "while (true) {}")]).await?;
    let error = tokio::time::timeout(
        Duration::from_secs(5),
        call_plugin(&plugins, "spin", json!({}), CancellationToken::new()),
    )
    .await?
    .unwrap_err();
    assert!(
        error.to_string().to_lowercase().contains("loop"),
        "{error:#}"
    );
    Ok(())
}

#[tokio::test]
async fn process_timeout_and_cancellation_do_not_block_executor() -> Result<()> {
    let plugins =
        Plugins::from_sources(vec![tool_source("exec", "return host.exec(input);")]).await?;
    let error = call_plugin(
        &plugins,
        "exec",
        json!({"program": "/bin/sleep", "args": ["10"], "timeoutMs": 20}),
        CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("timed out"));
    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    let (result, ()) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(
            call_plugin(
                &plugins,
                "exec",
                json!({"program": "/bin/sleep", "args": ["10"]}),
                cancel
            ),
            async move {
                tokio::time::sleep(Duration::from_millis(25)).await;
                stop.cancel();
            }
        )
    })
    .await?;
    assert!(result.unwrap_err().to_string().contains("cancelled"));
    Ok(())
}

#[tokio::test]
async fn directory_discovery_has_no_implicit_shell_and_reports_missing_directory() -> Result<()> {
    let directory = std::env::temp_dir().join(format!(
        "ngrc-plugins-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    ));
    std::fs::create_dir(&directory)?;
    let result = async {
        let plugins = Plugins::load(&directory).await?;
        assert!(plugins
            .tools(&runtime().load("main").await?)
            .await?
            .is_empty());
        std::fs::write(directory.join("ignore.txt"), "not JavaScript")?;
        std::fs::write(
            directory.join("custom.js"),
            tool_source("custom", "return input;").source,
        )?;
        let plugins = Plugins::load(&directory).await?;
        assert_eq!(
            plugins.tools(&runtime().load("main").await?).await?.len(),
            1
        );
        assert_eq!(
            call_plugin(
                &plugins,
                "custom",
                json!({"ok": true}),
                CancellationToken::new()
            )
            .await?,
            json!({"ok": true})
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    std::fs::remove_dir_all(&directory)?;
    result?;
    assert!(Plugins::load(&directory).await.is_err());
    Ok(())
}

struct PluginProvider(AtomicUsize);

#[async_trait]
impl Provider for PluginProvider {
    fn get_name(&self) -> &str {
        "plugins"
    }

    async fn stream(
        &self,
        _: &ModelConfig,
        _: &str,
        messages: &[Message],
        tools: &[Tool],
    ) -> Result<MessageStream, ProviderError> {
        assert_eq!(
            tools
                .iter()
                .map(|tool| tool.name.as_ref())
                .collect::<Vec<_>>(),
            ["echo", "fail"]
        );
        let message = match self.0.fetch_add(1, Ordering::SeqCst) {
            0 => Message::assistant()
                .with_tool_request(
                    "echo-1",
                    Ok(CallToolRequestParams::new("echo").with_arguments(
                        json!({"value": "plugin-ok"}).as_object().unwrap().clone(),
                    )),
                )
                .with_tool_request("fail-1", Ok(CallToolRequestParams::new("fail"))),
            1 => {
                let response = messages
                    .iter()
                    .find(|message| message.get_tool_response_ids().contains(&"echo-1"))
                    .unwrap();
                assert!(format!("{response:?}").contains("plugin-ok"));
                assert!(response.get_tool_response_ids().contains(&"fail-1"));
                assert!(format!("{response:?}").contains("intentional failure"));
                Message::assistant().with_text("plugins done")
            }
            n => panic!("unexpected call {n}"),
        };
        Ok(stream_from_single_message(
            message,
            ProviderUsage::new("test".into(), Default::default()),
        ))
    }
}

#[tokio::test]
async fn gdk_loop_handles_multiple_plugin_tools_and_errors() -> Result<()> {
    let plugins = Plugins::from_sources(vec![
        tool_source("echo", "return input;"),
        tool_source("fail", "throw new Error('intentional failure');"),
    ])
    .await?;
    let cancel = CancellationToken::new();
    let session = agent(
        Arc::new(PluginProvider(AtomicUsize::new(0))),
        ModelConfig::new("test"),
        cancel.clone(),
        plugins,
    )
    .run(&runtime(), "main", &emitter(cancel))
    .await?;
    assert_eq!(
        session.conversation.last().unwrap().as_concat_text(),
        "plugins done"
    );
    Ok(())
}

#[tokio::test]
async fn cancelled_gdk_turn_finalizes_plugin_tool_responses() -> Result<()> {
    let plugins = Plugins::from_sources(vec![
        tool_source(
            "echo",
            "return host.exec({program: '/bin/sleep', args: ['10']});",
        ),
        tool_source("fail", "return null;"),
    ])
    .await?;
    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    let runtime = runtime();
    let machine = agent(
        Arc::new(PluginProvider(AtomicUsize::new(0))),
        ModelConfig::new("test"),
        cancel.clone(),
        plugins,
    );
    let emit = emitter(cancel);
    let (session, ()) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(machine.run(&runtime, "main", &emit), async move {
            tokio::time::sleep(Duration::from_millis(40)).await;
            stop.cancel();
        })
    })
    .await?;
    let session = session?;
    for id in ["echo-1", "fail-1"] {
        assert!(session
            .conversation
            .iter()
            .any(|message| message.get_tool_response_ids().contains(&id)));
    }
    Ok(())
}
