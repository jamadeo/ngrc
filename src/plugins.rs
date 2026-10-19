//! Generic bridge for plugin tools and per-request context contributions.
//! GDK retains orchestration; Boa values never cross threads.
use std::{collections::BTreeMap, path::Path, sync::Arc};

use anyhow::{anyhow, bail, Context as _, Result};
use async_trait::async_trait;
use boa_engine::{
    js_string, object::ObjectInitializer, property::Attribute, Context, JsNativeError, JsObject,
    JsResult, JsValue, NativeFunction, Source,
};
use goose_agent::{operation::Emitter, tool::ToolProvider};
use rmcp::model::{CallToolRequestParams, CallToolResult, ErrorData, Tool};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::{host::HostState, Session};

const MAX_CONTEXT_SECTIONS: usize = 32;
const MAX_SECTION_BYTES: usize = 16 * 1024;
const MAX_CONTEXT_BYTES: usize = 64 * 1024;

#[derive(Clone)]
pub(crate) struct PluginSource {
    pub name: String,
    pub source: String,
}

enum Action {
    Tool {
        name: String,
        arguments: Value,
        request_id: String,
    },
    Context,
}

struct Invocation {
    action: Action,
    host: HostState,
    reply: oneshot::Sender<Result<Value, String>>,
}

struct Worker {
    name: String,
    sender: std::sync::mpsc::Sender<Invocation>,
    has_context: bool,
}

impl Worker {
    async fn invoke(&self, action: Action, host: HostState) -> Result<Value> {
        let cancel = host.cancel.clone();
        if cancel.is_cancelled() {
            bail!("plugin {}: invocation cancelled", self.name);
        }
        let (reply, result) = oneshot::channel();
        self.sender
            .send(Invocation {
                action,
                host,
                reply,
            })
            .with_context(|| format!("plugin {} worker stopped", self.name))?;
        tokio::select! {
            biased;
            _ = cancel.cancelled() => bail!("plugin {}: invocation cancelled", self.name),
            result = result => result
                .with_context(|| format!("plugin {} worker stopped", self.name))?
                .map_err(|error| anyhow!("plugin {}: {error}", self.name)),
        }
    }
}

struct RegisteredTool {
    definition: Tool,
    validator: jsonschema::Validator,
    worker: Arc<Worker>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Contribution {
    id: String,
    text: String,
}

#[derive(Default)]
pub(crate) struct Plugins {
    tools: BTreeMap<String, RegisteredTool>,
    workers: Vec<Arc<Worker>>,
}

impl Plugins {
    /// Load exactly this directory (no implicit tools or fallback), in path order.
    pub async fn load(directory: &Path) -> Result<Arc<Self>> {
        let mut paths = std::fs::read_dir(directory)
            .with_context(|| format!("read plugin directory {}", directory.display()))?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        paths.sort();
        let sources = paths
            .into_iter()
            .filter(|path| path.extension().is_some_and(|extension| extension == "js"))
            .map(|path| {
                Ok(PluginSource {
                    name: path.display().to_string(),
                    source: std::fs::read_to_string(&path)
                        .with_context(|| format!("read plugin {}", path.display()))?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Self::from_sources(sources).await
    }

    pub(crate) async fn from_sources(sources: Vec<PluginSource>) -> Result<Arc<Self>> {
        let mut registry = Self::default();
        for source in sources {
            let label = source.name.clone();
            let (sender, rx) = std::sync::mpsc::channel::<Invocation>();
            let (ready, loaded) = oneshot::channel();
            std::thread::Builder::new()
                .name(format!("plugin:{label}"))
                .spawn(move || {
                    let mut context = Context::default();
                    // Defense against accidental runaway scripts, not a security sandbox.
                    context
                        .runtime_limits_mut()
                        .set_loop_iteration_limit(1_000_000);
                    context.runtime_limits_mut().set_recursion_limit(128);
                    let (definitions, plugin) = match initialize(&source, &mut context) {
                        Ok(initialized) => initialized,
                        Err(error) => {
                            let _ = ready.send(Err(format!("{error:#}")));
                            return;
                        }
                    };
                    if ready
                        .send(Ok((definitions, plugin.context.is_some())))
                        .is_err()
                    {
                        return;
                    }
                    while let Ok(invocation) = rx.recv() {
                        let result = invoke(&plugin, &invocation, &mut context)
                            .map_err(|error| format!("{error:#}"));
                        let _ = invocation.reply.send(result);
                    }
                })?;
            let (definitions, has_context) = loaded
                .await
                .with_context(|| format!("plugin {label} worker stopped during initialization"))?
                .map_err(|error| anyhow!("plugin {label}: {error}"))?;
            let worker = Arc::new(Worker {
                name: label.clone(),
                sender,
                has_context,
            });
            for definition in definitions {
                let name = definition.name.to_string();
                if registry.tools.contains_key(&name) {
                    bail!("plugin {label}: duplicate tool name '{name}'");
                }
                let schema = Value::Object((*definition.input_schema).clone());
                let validator = jsonschema::validator_for(&schema).map_err(|error| {
                    anyhow!("plugin {label}, tool {name}: invalid inputSchema: {error}")
                })?;
                registry.tools.insert(
                    name,
                    RegisteredTool {
                        definition,
                        validator,
                        worker: worker.clone(),
                    },
                );
            }
            registry.workers.push(worker);
        }
        Ok(Arc::new(registry))
    }

    /// Rebuild request-local sections in plugin load order. Never persist them as messages.
    pub async fn context(&self, session: &Session, cancel: CancellationToken) -> Result<String> {
        let mut output = String::new();
        for worker in self.workers.iter().filter(|worker| worker.has_context) {
            let value = worker
                .invoke(
                    Action::Context,
                    HostState::new(session.cwd.clone(), cancel.clone(), false),
                )
                .await?;
            let sections: Vec<Contribution> = serde_json::from_value(value).with_context(|| {
                format!(
                    "plugin {}: context() must return an array of {{id, text}} sections",
                    worker.name
                )
            })?;
            if sections.len() > MAX_CONTEXT_SECTIONS {
                bail!(
                    "plugin {}: context() exceeds {MAX_CONTEXT_SECTIONS} sections",
                    worker.name
                );
            }
            let mut ids = std::collections::HashSet::new();
            for section in sections {
                if !valid_name(&section.id) || !ids.insert(section.id.clone()) {
                    bail!(
                        "plugin {}: invalid or duplicate context id '{}'",
                        worker.name,
                        section.id
                    );
                }
                if section.text.trim().is_empty() || section.text.len() > MAX_SECTION_BYTES {
                    bail!("plugin {}: context section '{}' must contain 1-{MAX_SECTION_BYTES} bytes of nonblank text", worker.name, section.id);
                }
                let heading = format!("\n\n## Plugin context: {} / {}\n", worker.name, section.id);
                if output.len() + heading.len() + section.text.len() > MAX_CONTEXT_BYTES {
                    bail!(
                        "plugin {}: context exceeds {MAX_CONTEXT_BYTES} bytes total across plugins",
                        worker.name
                    );
                }
                output.push_str(&heading);
                output.push_str(&section.text);
            }
        }
        Ok(output)
    }
}

#[async_trait]
impl ToolProvider<Session> for Plugins {
    async fn tools(&self, _: &Session) -> Result<Vec<Tool>> {
        Ok(self
            .tools
            .values()
            .map(|tool| tool.definition.clone())
            .collect())
    }

    async fn call(
        &self,
        session: &Session,
        request_id: &str,
        call: CallToolRequestParams,
        emit: &Emitter,
    ) -> Result<CallToolResult, ErrorData> {
        let tool = self.tools.get(call.name.as_ref()).ok_or_else(|| {
            ErrorData::invalid_params(format!("unknown plugin tool {}", call.name), None)
        })?;
        let arguments = Value::Object(call.arguments.unwrap_or_default());
        tool.validator
            .validate(&arguments)
            .map_err(|error| ErrorData::invalid_params(format!("{}: {error}", call.name), None))?;
        let value = tool
            .worker
            .invoke(
                Action::Tool {
                    name: call.name.to_string(),
                    arguments,
                    request_id: request_id.into(),
                },
                HostState::new(session.cwd.clone(), emit.cancel_token().clone(), true),
            )
            .await
            .map_err(|error| ErrorData::internal_error(format!("{error:#}"), None))?;
        Ok(CallToolResult::structured(value))
    }
}

fn js_error(error: impl std::fmt::Display) -> anyhow::Error {
    anyhow!(error.to_string())
}

fn host_state(context: &Context) -> JsResult<HostState> {
    context.get_data::<HostState>().cloned().ok_or_else(|| {
        JsNativeError::error()
            .with_message("host capabilities are only available during execute() or context()")
            .into()
    })
}

fn host_input<T: serde::de::DeserializeOwned>(
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<T> {
    let input = args
        .first()
        .unwrap_or(&JsValue::undefined())
        .to_json(context)?
        .ok_or_else(|| {
            JsNativeError::typ().with_message("host capability requires an options object")
        })?;
    serde_json::from_value(input)
        .map_err(|error| JsNativeError::typ().with_message(error.to_string()).into())
}

fn host_result(result: Result<Value>, context: &mut Context) -> JsResult<JsValue> {
    let output =
        result.map_err(|error| JsNativeError::error().with_message(format!("{error:#}")))?;
    JsValue::from_json(&output, context)
}

fn host_exec(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let input = host_input(args, context)?;
    host_result(host_state(context)?.exec(input), context)
}

fn host_read_file(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let input = host_input(args, context)?;
    host_result(host_state(context)?.read_file(input), context)
}

fn host_list_dir(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let input = host_input(args, context)?;
    host_result(host_state(context)?.list_dir(input), context)
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Definition {
    name: String,
    description: String,
    input_schema: serde_json::Map<String, Value>,
}

struct Handler {
    function: JsObject,
    receiver: JsValue,
}

struct Plugin {
    handlers: BTreeMap<String, Handler>,
    context: Option<Handler>,
}

fn optional_handler(
    object: &JsObject,
    name: &str,
    context: &mut Context,
) -> Result<Option<Handler>> {
    let value = object.get(js_string!(name), context).map_err(js_error)?;
    if value.is_undefined() {
        return Ok(None);
    }
    Ok(Some(Handler {
        function: value
            .as_callable()
            .ok_or_else(|| anyhow!("plugin.{name} must be a function"))?,
        receiver: object.clone().into(),
    }))
}

fn initialize(source: &PluginSource, context: &mut Context) -> Result<(Vec<Tool>, Plugin)> {
    let host = ObjectInitializer::new(context)
        .function(
            NativeFunction::from_fn_ptr(host_exec),
            js_string!("exec"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(host_read_file),
            js_string!("readFile"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(host_list_dir),
            js_string!("listDir"),
            1,
        )
        .build();
    context
        .register_global_property(js_string!("host"), host, Attribute::READONLY)
        .map_err(js_error)?;
    let plugin = context
        .eval(Source::from_bytes(&source.source))
        .map_err(js_error)?
        .as_object()
        .ok_or_else(|| anyhow!("script must evaluate to a plugin object"))?;
    let version = plugin
        .get(js_string!("apiVersion"), context)
        .map_err(js_error)?;
    if version.as_number() != Some(1.0) {
        bail!("expected apiVersion: 1");
    }
    let context_handler = optional_handler(&plugin, "context", context)?;
    let tool_handler = optional_handler(&plugin, "tools", context)?;
    let tools = match tool_handler {
        None => None,
        Some(handler) => {
            let value = handler
                .function
                .call(&handler.receiver, &[], context)
                .map_err(js_error)?;
            reject_promise(&value)?;
            Some(
                value
                    .as_object()
                    .filter(JsObject::is_array)
                    .ok_or_else(|| anyhow!("plugin.tools() must return an array"))?,
            )
        }
    };
    let mut definitions = Vec::new();
    let mut handlers = BTreeMap::new();
    if let Some(tools) = tools {
        let length = tools
            .get(js_string!("length"), context)
            .map_err(js_error)?
            .to_u32(context)
            .map_err(js_error)?;
        for index in 0..length {
            let tool = tools
                .get(index, context)
                .map_err(js_error)?
                .as_object()
                .ok_or_else(|| anyhow!("tool must be an object"))?;
            let execute = optional_handler(&tool, "execute", context)?
                .ok_or_else(|| anyhow!("tool.execute must be a function"))?;
            let mut value = serde_json::Map::new();
            for field in ["name", "description", "inputSchema"] {
                let field_value = tool.get(js_string!(field), context).map_err(js_error)?;
                value.insert(
                    field.into(),
                    field_value
                        .to_json(context)
                        .map_err(js_error)?
                        .ok_or_else(|| anyhow!("tool.{field} is required"))?,
                );
            }
            let definition: Definition = serde_json::from_value(Value::Object(value))?;
            if !valid_name(&definition.name) {
                bail!("tool names must be 1-64 ASCII letters, digits, '_' or '-'");
            }
            if definition.description.trim().is_empty()
                || definition.input_schema.get("type") != Some(&json!("object"))
            {
                bail!(
                    "tool {} requires a description and an object inputSchema",
                    definition.name
                );
            }
            if handlers.insert(definition.name.clone(), execute).is_some() {
                bail!("duplicate tool name '{}'", definition.name);
            }
            definitions.push(Tool::new(
                definition.name,
                definition.description,
                Arc::new(definition.input_schema),
            ));
        }
    }
    Ok((
        definitions,
        Plugin {
            handlers,
            context: context_handler,
        },
    ))
}

fn reject_promise(value: &JsValue) -> Result<()> {
    if value
        .as_object()
        .is_some_and(|object| object.is::<boa_engine::builtins::promise::Promise>())
    {
        bail!("async/Promise handlers are not supported in plugin API v1");
    }
    Ok(())
}

fn invoke(plugin: &Plugin, invocation: &Invocation, context: &mut Context) -> Result<Value> {
    if invocation.host.cancel.is_cancelled() {
        bail!("plugin invocation cancelled");
    }
    let (handler, args, label) = match &invocation.action {
        Action::Tool {
            name,
            arguments,
            request_id,
        } => {
            let handler = plugin
                .handlers
                .get(name)
                .ok_or_else(|| anyhow!("unknown tool"))?;
            let input = JsValue::from_json(arguments, context).map_err(js_error)?;
            let info = JsValue::from_json(
                &json!({
                    "cwd": invocation.host.cwd.to_string_lossy(), "requestId": request_id,
                }),
                context,
            )
            .map_err(js_error)?;
            (handler, vec![input, info], "tool.execute")
        }
        Action::Context => {
            let handler = plugin
                .context
                .as_ref()
                .ok_or_else(|| anyhow!("no context entrypoint"))?;
            let info = JsValue::from_json(
                &json!({"cwd": invocation.host.cwd.to_string_lossy()}),
                context,
            )
            .map_err(js_error)?;
            (handler, vec![info], "context()")
        }
    };
    context.insert_data(invocation.host.clone());
    // Keep host capabilities available during result conversion too (e.g. getters).
    let result = (|| {
        let value = handler
            .function
            .call(&handler.receiver, &args, context)
            .map_err(js_error)?;
        reject_promise(&value)?;
        value
            .to_json(context)
            .map_err(js_error)?
            .ok_or_else(|| anyhow!("{label} must return a JSON value, not undefined"))
    })();
    context.remove_data::<HostState>();
    result
}
