//! The only bridge between model-facing tools and JavaScript plugins.
//! GDK retains request/response bookkeeping; Boa values never cross threads.
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

use crate::{host::HostState, Session};

#[derive(Clone)]
pub(crate) struct PluginSource {
    pub name: String,
    pub source: String,
}

struct Invocation {
    name: String,
    arguments: Value,
    request_id: String,
    host: HostState,
    reply: oneshot::Sender<Result<Value, String>>,
}

struct RegisteredTool {
    definition: Tool,
    validator: jsonschema::Validator,
    worker: std::sync::mpsc::Sender<Invocation>,
}

#[derive(Default)]
pub(crate) struct Plugins {
    tools: BTreeMap<String, RegisteredTool>,
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
            let (worker, rx) = std::sync::mpsc::channel::<Invocation>();
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
                    let initialized = initialize(&source, &mut context);
                    let (definitions, handlers) = match initialized {
                        Ok(initialized) => initialized,
                        Err(error) => {
                            let _ = ready.send(Err(format!("{error:#}")));
                            return;
                        }
                    };
                    if ready.send(Ok(definitions)).is_err() {
                        return;
                    }
                    while let Ok(invocation) = rx.recv() {
                        let result = invoke(&handlers, &invocation, &mut context)
                            .map_err(|error| format!("{}: {error:#}", source.name));
                        let _ = invocation.reply.send(result);
                    }
                })?;
            let definitions = loaded
                .await
                .with_context(|| format!("plugin {label} worker stopped during initialization"))?
                .map_err(|error| anyhow!("plugin {label}: {error}"))?;
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
        }
        Ok(Arc::new(registry))
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
        let (reply, result) = oneshot::channel();
        tool.worker
            .send(Invocation {
                name: call.name.to_string(),
                arguments,
                request_id: request_id.to_owned(),
                host: HostState {
                    cwd: session.cwd.clone(),
                    cancel: emit.cancel_token().clone(),
                    runtime: tokio::runtime::Handle::current(),
                },
                reply,
            })
            .map_err(|_| ErrorData::internal_error("plugin worker stopped", None))?;
        let value = result
            .await
            .map_err(|_| ErrorData::internal_error("plugin worker stopped", None))?
            .map_err(|error| ErrorData::internal_error(error, None))?;
        Ok(CallToolResult::structured(value))
    }
}

fn js_error(error: impl std::fmt::Display) -> anyhow::Error {
    anyhow!(error.to_string())
}

fn host_exec(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let input = args
        .first()
        .unwrap_or(&JsValue::undefined())
        .to_json(context)?
        .ok_or_else(|| JsNativeError::typ().with_message("host.exec requires an options object"))?;
    let input = serde_json::from_value(input)
        .map_err(|error| JsNativeError::typ().with_message(error.to_string()))?;
    let host = context.get_data::<HostState>().cloned().ok_or_else(|| {
        JsNativeError::error().with_message("host.exec is only available during tool execution")
    })?;
    let output = host
        .exec(input)
        .map_err(|error| JsNativeError::error().with_message(format!("{error:#}")))?;
    JsValue::from_json(&output, context)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Definition {
    name: String,
    description: String,
    input_schema: serde_json::Map<String, Value>,
}

fn initialize(
    source: &PluginSource,
    context: &mut Context,
) -> Result<(Vec<Tool>, BTreeMap<String, JsObject>)> {
    let host = ObjectInitializer::new(context)
        .function(
            NativeFunction::from_fn_ptr(host_exec),
            js_string!("exec"),
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
    let tools = plugin
        .get(js_string!("tools"), context)
        .map_err(js_error)?
        .as_object()
        .filter(JsObject::is_array)
        .ok_or_else(|| anyhow!("plugin.tools must be an array"))?;
    let length = tools
        .get(js_string!("length"), context)
        .map_err(js_error)?
        .to_u32(context)
        .map_err(js_error)?;
    let mut definitions = Vec::new();
    let mut handlers = BTreeMap::new();
    for index in 0..length {
        let tool = tools
            .get(index, context)
            .map_err(js_error)?
            .as_object()
            .ok_or_else(|| anyhow!("tool must be an object"))?;
        let execute = tool
            .get(js_string!("execute"), context)
            .map_err(js_error)?
            .as_callable()
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
        if definition.name.is_empty()
            || definition.name.len() > 64
            || !definition
                .name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
        {
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
    Ok((definitions, handlers))
}

fn invoke(
    handlers: &BTreeMap<String, JsObject>,
    invocation: &Invocation,
    context: &mut Context,
) -> Result<Value> {
    if invocation.host.cancel.is_cancelled() {
        bail!("tool call cancelled");
    }
    let handler = handlers
        .get(&invocation.name)
        .ok_or_else(|| anyhow!("unknown tool"))?;
    let arguments = JsValue::from_json(&invocation.arguments, context).map_err(js_error)?;
    let info = JsValue::from_json(
        &json!({
            "cwd": invocation.host.cwd.to_string_lossy(),
            "requestId": invocation.request_id,
        }),
        context,
    )
    .map_err(js_error)?;
    context.insert_data(invocation.host.clone());
    let result = handler.call(&JsValue::undefined(), &[arguments, info], context);
    context.remove_data::<HostState>();
    let result = result.map_err(js_error)?;
    if result
        .as_object()
        .is_some_and(|object| object.is::<boa_engine::builtins::promise::Promise>())
    {
        bail!("async/Promise tool handlers are not supported in plugin API v1");
    }
    result
        .to_json(context)
        .map_err(js_error)?
        .ok_or_else(|| anyhow!("tool.execute must return a JSON value, not undefined"))
}
