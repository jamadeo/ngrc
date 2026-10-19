use super::*;
use plugins::PluginSource;

struct Workspace(PathBuf);
impl Workspace {
    fn new() -> Result<Self> {
        let path = std::env::temp_dir().join(format!(
            "ngrc-context-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        std::fs::create_dir(&path)?;
        Ok(Self(path))
    }
    fn session(&self) -> Session {
        Session {
            cwd: self.0.clone(),
            conversation: Conversation::new_unvalidated([Message::user().with_text("test")]),
        }
    }
    fn write(&self, path: &str, text: &str) -> Result<()> {
        std::fs::write(self.0.join(path), text)?;
        Ok(())
    }
}
impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn source(name: &str, script: &str) -> PluginSource {
    PluginSource {
        name: name.into(),
        source: script.into(),
    }
}
async fn context_for(script: &str, session: &Session) -> Result<String> {
    Plugins::from_sources(vec![source("test.js", script)])
        .await?
        .context(session, CancellationToken::new())
        .await
}
async fn project_plugins() -> Result<Arc<Plugins>> {
    Plugins::from_sources(vec![source(
        "project.js",
        include_str!("../../plugins/project.js"),
    )])
    .await
}

#[tokio::test]
async fn optional_entrypoints_and_context_only_worker_survive() -> Result<()> {
    let session = runtime().load("main").await?;
    assert!(context_for("({apiVersion: 1})", &session).await?.is_empty());
    let plugins = Plugins::from_sources(vec![source("context-only.js", r#"
        let count = 0;
        ({apiVersion: 1, context(ctx) { return [{id: 'count', text: String(++count) + ':' + ctx.cwd}]; }})
    "#)]).await?;
    assert!(plugins.tools(&session).await?.is_empty());
    for count in [1, 2] {
        let text = plugins.context(&session, CancellationToken::new()).await?;
        assert!(text.contains(&format!("{count}:{}", session.cwd.display())));
    }
    Ok(())
}

#[tokio::test]
async fn context_order_namespacing_and_tool_registration_once() -> Result<()> {
    let plugins = Plugins::from_sources(vec![source("z.js", r#"
        ({apiVersion: 1, registrations: 0,
            tools() { this.registrations++; return [{name: 'count', description: 'Count', inputSchema: {type: 'object'}, execute() {return this.answer;}, answer: 42}]; },
            context() { return [{id: 'same', text: String(this.registrations)}]; }
        })
    "#), source("a.js", "({apiVersion: 1, context() {return [{id: 'same', text: 'second'}];}})")]).await?;
    let session = runtime().load("main").await?;
    for _ in 0..2 {
        plugins.tools(&session).await?;
        let text = plugins.context(&session, CancellationToken::new()).await?;
        assert!(text.find("z.js / same\n1").unwrap() < text.find("a.js / same\nsecond").unwrap());
    }
    assert_eq!(
        call_plugin(&plugins, "count", json!({}), CancellationToken::new()).await?,
        42
    );
    Ok(())
}

#[tokio::test]
async fn invalid_context_and_limits_are_reported_with_filename() -> Result<()> {
    let session = runtime().load("main").await?;
    for expression in [
        "undefined",
        "null",
        "{}",
        "Promise.resolve([])",
        "[{id: 'x'}]",
        "[{id: 'x', text: 1}]",
        "[{id: '', text: 'x'}]",
        "[{id: 'bad id', text: 'x'}]",
        "[{id: 'x', text: '   '}]",
        "[{id: 'x', text: 'x', extra: true}]",
        "[{id: 'x', text: 'a'}, {id: 'x', text: 'b'}]",
        "[{id: 'x', text: 'a'.repeat(16385)}]",
        "Array.from({length: 33}, (_, i) => ({id: 's' + i, text: 'x'}))",
    ] {
        let script = format!("({{apiVersion: 1, context() {{return {expression};}}}})");
        let error = context_for(&script, &session).await.unwrap_err();
        assert!(
            format!("{error:#}").contains("test.js"),
            "{expression}: {error:#}"
        );
    }
    let script = "({apiVersion: 1, context() {return Array.from({length: 5}, (_, i) => ({id: 's' + i, text: 'x'.repeat(16384)}));}})";
    assert!(context_for(script, &session)
        .await
        .unwrap_err()
        .to_string()
        .contains("total"));
    Ok(())
}

#[tokio::test]
async fn context_cannot_execute_processes_and_errors_can_be_retried() -> Result<()> {
    let session = runtime().load("main").await?;
    let error = context_for(
        "({apiVersion: 1, context() { host.exec({program: '/bin/echo'}); return []; }})",
        &session,
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("not context()"), "{error:#}");
    let plugins = Plugins::from_sources(vec![source("retry.js", "let n = 0; ({apiVersion: 1, context() { if (++n === 1) throw new Error('oops'); return []; }})")]).await?;
    assert!(plugins
        .context(&session, CancellationToken::new())
        .await
        .is_err());
    assert!(plugins
        .context(&session, CancellationToken::new())
        .await?
        .is_empty());
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(plugins.context(&session, cancel).await.is_err());
    Ok(())
}

#[tokio::test]
async fn filesystem_host_is_bounded_sorted_and_relative() -> Result<()> {
    let workspace = Workspace::new()?;
    workspace.write("a.txt", "abcdef")?;
    workspace.write("b.txt", "test")?;
    let script = r#"({apiVersion: 1, context() {
        const file = host.readFile({path: 'a.txt', maxBytes: 3});
        const listing = host.listDir({maxEntries: 1});
        return [{id: 'result', text: JSON.stringify({file, listing, missing: host.readFile({path: 'absent'})})}];
    }})"#;
    let text = context_for(script, &workspace.session()).await?;
    assert!(text.contains(r#""text":"abc","truncated":true"#));
    assert!(text.contains(r#""name":"a.txt""#));
    assert!(!text.contains("b.txt"));
    assert!(text.contains(r#""missing":null"#));
    for call in [
        "host.readFile({path: '../outside'})",
        "host.readFile({path: '/etc/passwd'})",
        "host.readFile({path: 'a.txt', maxBytes: 0})",
        "host.readFile({path: 'a.txt', maxBytes: 65537})",
        "host.readFile({path: '.'})",
        "host.listDir({maxEntries: 201})",
        "host.listDir({maxEntries: 0})",
        "host.listDir({path: '..'})",
        "host.readFile({path: 'a.txt', extra: 1})",
    ] {
        let script = format!("({{apiVersion: 1, context() {{{call}; return [];}}}})");
        assert!(
            context_for(&script, &workspace.session()).await.is_err(),
            "{call}"
        );
    }
    let error = context_for("({apiVersion: 1, context() { for (let i=0;i<65;i++) host.readFile({path: 'absent'}); return []; }})", &workspace.session()).await.unwrap_err();
    assert!(error.to_string().contains("read budget"));
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn filesystem_reads_reject_symlinks() -> Result<()> {
    let workspace = Workspace::new()?;
    std::os::unix::fs::symlink("/etc/passwd", workspace.0.join("link"))?;
    std::os::unix::fs::symlink("/etc", workspace.0.join("dir"))?;
    for path in ["link", "dir/passwd"] {
        let script = format!(
            "({{apiVersion: 1, context() {{host.readFile({{path: {path:?}}}); return [];}}}})"
        );
        let error = context_for(&script, &workspace.session())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("symlink"));
    }
    Ok(())
}

#[tokio::test]
async fn project_plugin_collects_and_refreshes_without_tools_or_execution() -> Result<()> {
    let workspace = Workspace::new()?;
    workspace.write("AGENTS.md", "Use project conventions. 雪")?;
    workspace.write("Cargo.toml", "[package]\nname = 'test'")?;
    workspace.write(
        "package.json",
        r#"{"scripts":{"build":"DO NOT EXECUTE", "test":"DO NOT EXECUTE"}}"#,
    )?;
    workspace.write("pnpm-lock.yaml", "lockfileVersion: 9")?;
    workspace.write("pyproject.toml", "[project]")?;
    workspace.write("go.mod", "module example")?;
    std::fs::create_dir(workspace.0.join("src"))?;
    std::fs::create_dir(workspace.0.join("node_modules"))?;
    workspace.write("node_modules/secret", "not context")?;
    let plugins = project_plugins().await?;
    assert!(plugins.tools(&workspace.session()).await?.is_empty());
    let first = plugins
        .context(&workspace.session(), CancellationToken::new())
        .await?;
    for expected in [
        "Use project conventions. 雪",
        "cargo test",
        "pnpm run build",
        "pnpm run test",
        "python3 -m pytest",
        "go test ./...",
        "\"src\"/",
    ] {
        assert!(first.contains(expected), "missing {expected}: {first}");
    }
    assert!(!first.contains("DO NOT EXECUTE"));
    assert!(!first.contains("node_modules"));
    workspace.write("AGENTS.md", "New guidance")?;
    let next = plugins
        .context(&workspace.session(), CancellationToken::new())
        .await?;
    assert!(next.contains("New guidance"));
    assert!(!next.contains("Use project conventions"));
    workspace.write("package.json", "{broken")?;
    assert!(plugins
        .context(&workspace.session(), CancellationToken::new())
        .await?
        .contains("invalid JSON"));
    Ok(())
}

#[tokio::test]
async fn project_plugin_handles_empty_and_large_workspaces() -> Result<()> {
    let workspace = Workspace::new()?;
    let plugins = project_plugins().await?;
    let text = plugins
        .context(&workspace.session(), CancellationToken::new())
        .await?;
    assert!(text.contains("(empty)"));
    assert!(!text.contains("commands\n"));
    workspace.write("AGENTS.md", &"x".repeat(20000))?;
    workspace.write("package.json", &"x".repeat(20000))?;
    for i in 0..100 {
        workspace.write(&format!("entry-{i:03}"), "")?;
    }
    let text = plugins
        .context(&workspace.session(), CancellationToken::new())
        .await?;
    assert!(text.contains("[truncated]"));
    assert!(text.contains("[list truncated]"));
    assert!(text.len() < 16000);
    Ok(())
}

struct RefreshProvider(AtomicUsize);
#[async_trait]
impl Provider for RefreshProvider {
    fn get_name(&self) -> &str {
        "context-refresh"
    }
    async fn stream(
        &self,
        _: &ModelConfig,
        system: &str,
        messages: &[Message],
        tools: &[Tool],
    ) -> Result<MessageStream, ProviderError> {
        let n = self.0.fetch_add(1, Ordering::SeqCst) + 1;
        assert_eq!(system.matches("fresh-context-").count(), 1);
        assert!(system.contains(&format!("fresh-context-{n}")));
        assert!(messages
            .iter()
            .all(|message| !format!("{message:?}").contains("fresh-context-")));
        assert_eq!(tools.len(), 1);
        let message = if n == 1 {
            Message::assistant().with_tool_request(
                "shell-refresh",
                Ok(CallToolRequestParams::new("shell")
                    .with_arguments(json!({"command": "printf ok"}).as_object().unwrap().clone())),
            )
        } else {
            Message::assistant().with_text("done")
        };
        Ok(stream_from_single_message(
            message,
            ProviderUsage::new("test".into(), Default::default()),
        ))
    }
}

#[tokio::test]
async fn context_is_refreshed_each_inference_and_never_persisted_in_chat() -> Result<()> {
    let plugins = Plugins::from_sources(vec![
        source("shell.js", include_str!("../../plugins/shell.js")),
        source("context.js", "let n=0; ({apiVersion: 1, context() { return [{id: 'fresh', text: 'fresh-context-' + (++n)}];}})"),
    ]).await?;
    let runtime = empty_runtime();
    let provider = Arc::new(RefreshProvider(AtomicUsize::new(0)));
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    tx.send(Ok("follow up".into())).await?;
    drop(tx);
    chat(
        &runtime,
        plugins,
        provider.clone(),
        ModelConfig::new("test"),
        Some("first".into()),
        rx,
        CancellationToken::new(),
    )
    .await?;
    assert_eq!(provider.0.load(Ordering::SeqCst), 3);
    assert!(!format!("{:?}", runtime.load("main").await?.conversation).contains("fresh-context-"));
    Ok(())
}

#[tokio::test]
async fn bundled_directory_loads_shell_and_project() -> Result<()> {
    let plugins = Plugins::load(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("plugins")).await?;
    let workspace = Workspace::new()?;
    let tools = plugins.tools(&workspace.session()).await?;
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name.as_ref(), "shell");
    assert!(plugins
        .context(&workspace.session(), CancellationToken::new())
        .await?
        .contains("project.js / overview"));
    Ok(())
}

#[tokio::test]
async fn project_detects_package_manager_and_only_existing_scripts() -> Result<()> {
    for (lock, runner) in [
        (None, "npm"),
        (Some("yarn.lock"), "yarn"),
        (Some("bun.lock"), "bun"),
        (Some("bun.lockb"), "bun"),
    ] {
        let workspace = Workspace::new()?;
        workspace.write(
            "package.json",
            r#"{"scripts":{"test":"not executed","lint":"not executed"}}"#,
        )?;
        if let Some(lock) = lock {
            workspace.write(lock, "lock")?;
        }
        let plugins = project_plugins().await?;
        let text = plugins
            .context(&workspace.session(), CancellationToken::new())
            .await?;
        assert!(text.contains(&format!("{runner} run test")));
        assert!(text.contains(&format!("{runner} run lint")));
        assert!(!text.contains("run build"));
    }
    Ok(())
}

#[tokio::test]
async fn context_failure_prevents_inference() -> Result<()> {
    let plugins = Plugins::from_sources(vec![source(
        "broken-context.js",
        "({apiVersion: 1, context() {throw new Error('context failed');}})",
    )])
    .await?;
    let provider = Arc::new(ScriptedProvider(AtomicUsize::new(0)));
    let cancel = CancellationToken::new();
    let error = agent(
        provider.clone(),
        ModelConfig::new("test"),
        cancel.clone(),
        plugins,
    )
    .run(&runtime(), "main", &emitter(cancel))
    .await
    .err()
    .expect("preparation must fail");
    assert!(error.to_string().contains("broken-context.js"));
    assert_eq!(provider.0.load(Ordering::SeqCst), 0);
    Ok(())
}
