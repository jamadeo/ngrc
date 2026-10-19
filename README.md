# ngrc

A small Rust coding agent with JavaScript tool plugins, executed in-process by
[Boa](https://boajs.dev/). The Goose GDK still supplies the state-machine agent
loop, provider implementation, and streaming inference. There are **no hardcoded
model-facing tools**: even `shell` is a plugin.

One interactive chat, one in-memory conversation. Nothing saves the conversation
or plugin state to disk; both disappear when the process exits. Requires Rust
1.94.1+; the bundled shell plugin also requires `/bin/bash` (macOS/Linux).

## Boundaries

```text
CLI / in-memory session
          │
GDK StateMachine
   ├── ToolOperation ── generic Plugins provider ── Boa plugin workers
   │                                              └── host.exec → OS process
   └── InferenceRunner / OpenAiProvider
```

- **GDK owns orchestration:** inference, tool-request/response bookkeeping,
  conversation effects, and cancellation finalization. This is not a new agent
  loop implemented in JavaScript.
- **The plugin bridge owns the contract:** discover `.js` files, validate and
  snapshot their tool definitions, advertise schemas, validate call arguments,
  dispatch to the corresponding handler, and convert JSON results/errors back
  into GDK tool responses. It knows nothing about Bash or coding tools.
- **Plugins own tools:** names, descriptions, JSON Schemas, argument handling,
  implementation, and result formatting. Multiple tools may share one plugin.
- **The Rust host owns OS capabilities:** currently only process execution. It
  does not register a tool of its own. The bundled `shell.js` chooses Bash and its
  arguments; Rust only executes a program with an argument vector.
- **The application owns chat/session policy:** prompt, current directory,
  rendering, provider configuration, and an in-memory conversation.

Implementation: `src/main.rs` (application/GDK assembly), `src/plugins.rs`
(plugin bridge and Boa workers), `src/host.rs` (OS boundary), and `plugins/`
(model-facing tools).

## Running this project

Set `OPENAI_API_KEY` and `NGRC_MODEL` in your environment. The model must support
tool calling; there is deliberately no hardcoded model default. From this directory:

```sh
cargo run --locked
# Or send an initial message before entering the same chat:
cargo run --locked -- "Inspect this project and summarize it without changing files"
```

`NGRC_PLUGIN_DIR` selects the plugin directory; it defaults to `./plugins`, relative
to the process's current directory. Only immediate files with the `.js` extension
are loaded, in sorted path order. There is no implicit built-in/fallback shell.
An empty directory gives an agent with no tools; a missing directory, invalid
plugin, invalid schema, or duplicate tool name fails startup. Plugins are loaded
once, not hot-reloaded. Tool definitions remain fixed for the chat.

With no arguments, the app starts at a `>` prompt without making a model request.
Each nonblank input line is a message. Earlier messages, replies, and tool results
remain in context. EOF (Ctrl-D) exits; Ctrl-C cancels an active turn or idle input.
Provider errors return to the prompt so a follow-up can retry. Assistant text
streams to stdout; diagnostics, process invocations, and usage go to stderr.

These commands and `NGRC_*` variables are specific to this project, not Goose CLI
configuration. No Goose CLI configuration or external MCP server is required.

## Plugin API v1

A plugin is a plain JavaScript **script whose final expression evaluates to the
plugin object**. It is not a Node.js package, CommonJS module, or ES module.
No `module.exports`, `export default`, or imports. For example, save this as
`plugins/echo.js`:

```js
({
  apiVersion: 1,
  tools: [
    {
      name: "echo",
      description: "Return the supplied text.",
      inputSchema: {
        type: "object",
        properties: { text: { type: "string" } },
        required: ["text"],
        additionalProperties: false,
      },
      execute(input, context) {
        return { text: input.text };
      },
    },
  ],
})
```

Contract:

- `apiVersion` must be the number `1`; `tools` must be an array (possibly empty).
- Each tool requires `name`, a nonblank `description`, an `inputSchema` with
  `type: "object"`, and an `execute` function. Names are globally unique across
  plugins: 1–64 ASCII letters, digits, underscores, or hyphens.
- `inputSchema` is JSON Schema. Schemas are compiled at load time and arguments
  are validated **before** calling JavaScript. Remote/file schema retrieval is
  disabled; use inline schemas or references within the schema.
- `execute(input, context)` is synchronous and receives JSON arguments plus
  `{ cwd, requestId }`. It must return a JSON-compatible value: an object, array,
  string, number, boolean, or `null`. Return plain data, not functions or class
  instances. `undefined`, cyclic results, and Promises are errors. `async`
  handlers/Promise execution are not supported; there is no job/event loop.
- Throw an `Error` to report a failed tool call. Validation failures, exceptions,
  and host errors become correlated tool errors for the model, not CLI crashes.
- Each file has its own persistent Boa context on a dedicated thread. Its tools
  share closure/global state, which survives subsequent calls and chat turns.
  Files do not share JavaScript globals. Calls within a plugin are serialized;
  JavaScript execution does not block the Tokio executor. No Boa values cross
  threads.
- There is no conversation mutation, inference access, inter-plugin messaging,
  lifecycle hook system, permission manifest, or package manager in this version.
  The tool contract and host-capability boundary are the extension points for
  future iterations, rather than exposing the whole agent internals today.

### Host capability: `host.exec(options)`

Available during tool execution only (not plugin initialization):

```js
const result = host.exec({
  program: "/bin/bash",
  args: ["-c", input.command],
  timeoutMs: 120000,
});
// result: { stdout: string, stderr: string, exit_code: number | null }
```

`program` is required and nonempty; `args` defaults to `[]`; `timeoutMs` defaults
to `120000` and must be an integer from 1 to 120000. Unknown options are rejected.
The host uses an argument vector; it does not interpret shell syntax. Commands
start in the session directory with inherited environment, closed stdin, and
captured stdout/stderr. A nonzero process exit is returned as data; spawn failures,
timeouts, and cancellation throw a JavaScript error. A plugin can inspect or
transform the result. Directory and environment changes do not persist between
processes.

`plugins/shell.js` is the complete bundled shell implementation and an example of
building a coding tool entirely with this API.

## Trust and prototype limits

**Only load trusted plugins and use a trusted/disposable workspace.** Plugins can
execute arbitrary programs immediately with your user permissions and inherited
environment, including credentials. Loading plugins from a project directory is
not safe for untrusted repositories. There is no approval prompt, filesystem or
network sandbox, or capability permission enforcement. Boa context isolation
separates plugin state; it is **not** a security boundary.

Host processes have a maximum 120-second timeout and observe cancellation; the
direct child is killed when its execution future is dropped. Descendant processes
are not reliably terminated. Output is buffered without a size cap. Boa has a
1,000,000-iteration loop limit and a recursion limit of 128 to catch common runaway
scripts, but there is no complete execution-time or memory budget. Cancellation
can finalize the agent turn without waiting for arbitrary JavaScript to stop;
JavaScript is not forcibly interrupted. There is no context compaction or model
turn/cost limit. These are prototype limitations, not production safety guarantees.

## Verification

```sh
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

Tests use scripted providers and local Boa/process execution; no paid/live model
call is required. They cover the model → plugin → model loop, multiple tool calls,
error responses, schema validation, malformed plugins, duplicate names, persistent
and isolated plugin state, directory discovery, no implicit tools, process output,
Bash semantics, cwd/stdin behavior, timeout/cancellation, runaway loops, follow-up
conversation context, optional initial input, EOF/blank input, idle cancellation,
and provider-error retries. Authenticated live inference has not been tested.

## GDK references / verification completed

Pinned to `0.1.0-alpha.12` of `goose-agent`, `goose-providers`, and
`goose-provider-types`, and Boa `0.22.0`, with `Cargo.lock` checked in.
`goose-providers` explicitly enables `rustls-tls` for HTTPS. Published GDK source
was consulted and the assembly verified by compilation and scripted-provider tests:

- [`goose-agent` machine](https://github.com/aaif-goose/goose/blob/4811ae25f736fdf0ede847449c060e96645426b5/crates/goose-agent/src/machine.rs)
  — `StateMachine`, operation steps, session/effect handling, cancellation.
- [`goose-agent` tools](https://github.com/aaif-goose/goose/blob/4811ae25f736fdf0ede847449c060e96645426b5/crates/goose-agent/src/tool.rs)
  — dynamic `ToolProvider` and `ToolOperation::with_provider`; no duplicate
  request/response loop in the app.
- [`goose-agent` inference](https://github.com/aaif-goose/goose/blob/4811ae25f736fdf0ede847449c060e96645426b5/crates/goose-agent/src/inference.rs)
  — `InferenceRunner`, request preparation, custom `InferenceEffect`.
- [`goose-providers` OpenAI](https://github.com/aaif-goose/goose/blob/4811ae25f736fdf0ede847449c060e96645426b5/crates/goose-providers/src/openai.rs)
  — provider implementation remains unchanged.

Official documentation consulted for context:

- [Configure LLM Provider](https://goose-docs.ai/docs/getting-started/providers)
  — OpenAI and `OPENAI_API_KEY`.
- [Using Extensions](https://goose-docs.ai/docs/getting-started/using-extensions)
  — Goose's extensions use MCP. This project's Boa plugin API is deliberately a
  separate, project-owned in-process tool contract, **not** a Goose extension API.
