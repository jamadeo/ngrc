# ngrc

A small Rust coding agent with JavaScript tool/context plugins, executed in-process by
[Boa](https://boajs.dev/). The Goose GDK still supplies the state-machine agent
loop, provider implementation, and streaming inference. There are **no hardcoded
model-facing tools**: `shell` provides tools and `project` supplies workspace context.

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
          └── request preparer ── Plugins.context ── Boa context handlers
                                                     └── bounded host filesystem reads
```

- **GDK owns orchestration:** inference, tool-request/response bookkeeping,
  conversation effects, and cancellation finalization. This is not a new agent
  loop implemented in JavaScript.
- **The plugin bridge owns the contract:** discover `.js` files, validate and
  snapshot their tool definitions, advertise schemas, validate call arguments,
  dispatch to the corresponding handler, and convert JSON results/errors back
  into GDK tool responses. It also rebuilds bounded context sections for each
  inference request. It knows nothing about Bash, project conventions, or manifests.
- **Plugins own tools and context:** names, descriptions, JSON Schemas, handlers,
  result formatting, project guidance, and workspace knowledge. A plugin can
  provide tools, context, both, or neither.
- **The Rust host owns OS capabilities:** process execution and bounded filesystem
  reads. It does not register tools of its own. `shell.js` chooses Bash and its
  arguments; `project.js` decides which files to read and how to interpret them.
- **The application owns chat/session policy:** prompt, current directory,
  rendering, provider configuration, and an in-memory conversation.

Implementation: `src/main.rs` (application/GDK assembly), `src/plugins.rs`
(plugin bridge and Boa workers), `src/host.rs` (OS boundary), and `plugins/`
(bundled tools and context).

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
streams to stdout; diagnostics and process invocations go to stderr. Usage
reporting is currently disabled.

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
  tools() {
    return [
      {
        name: "echo",
        description: "Return the supplied text.",
        inputSchema: {
          type: "object",
          properties: { text: { type: "string" } },
          required: ["text"],
          additionalProperties: false,
        },
        execute(input, ctx) {
          return { text: input.text };
        },
      },
    ];
  },
  context(ctx) {
    return [{ id: "guidance", text: "Use the echo tool for echo requests." }];
  },
})
```

`apiVersion` must be the number `1`. Both entrypoints are optional, but must be
functions if present. This replaces the earlier prototype's `tools: [...]` array;
there is no backwards-compatibility adapter. The API remains v1 while prototyping.

### `tools()` — static tool contributions

- Called once at load time, without arguments or host access. Returns an array
  (possibly empty); definitions and handler references are captured for the chat.
- Each tool requires `name`, a nonblank `description`, an `inputSchema` with
  `type: "object"`, and an `execute` function. Names are globally unique across
  plugins: 1–64 ASCII letters, digits, underscores, or hyphens.
- `inputSchema` is JSON Schema. Schemas are compiled at load time and arguments
  are validated **before** calling JavaScript. Remote/file schema retrieval is
  disabled; use inline schemas or references within the schema.
- `execute(input, ctx)` receives JSON arguments plus `{ cwd, requestId }` and must
  return plain JSON-compatible data. Throw an `Error` to report a failed tool call.
  Validation failures, exceptions, and host errors become correlated tool errors.

### `context(ctx)` — fresh context before each inference

- Receives `{ cwd }` and returns an array of `{ id, text }` sections. Returning `[]`
  contributes nothing. Context-only plugins do not register any tools.
- Contributions are regenerated before **every** inference, including after tool
  calls and in follow-up turns. They supplement the core system prompt only for
  that request; they are never appended to conversation history.
- Composition follows sorted plugin-file order, then returned array order. Section
  headings include the plugin filename and section ID. IDs use the same syntax as
  tool names and must be unique within a plugin's response; different plugins can
  use the same ID.
- Limits: 32 sections per plugin, 16 KiB UTF-8 text per section, 64 KiB total across
  plugins including headings. Text must be nonblank. Invalid output, oversized
  contributions, and exceptions fail request preparation with a diagnostic rather
  than silently dropping context. A context failure currently ends the chat;
  unlike provider/tool errors it is a runtime/configuration error.
- Filesystem reads are available; process execution is rejected during context
  collection. Project metadata is labeled as data, and project guidance is
  subordinate to core instructions. These labels are not a prompt-injection defense.

All functions are synchronous. `undefined`, cyclic results, and Promises are
errors; there is no async/Promise job loop, imports, Node.js APIs, or package manager.
Return plain data, not functions or class instances. Each file has its own
persistent Boa context on a dedicated thread; tool and context handlers share
closure/global state across turns. Calls within a plugin are serialized. No Boa
values cross threads, and JavaScript does not block the Tokio executor.

There are intentionally no setup/dispose, step, commands, events, direct
conversation mutation, inference access, or inter-plugin messaging APIs.

### Bundled plugins

- **`shell.js`**: tool-only; exposes the non-interactive Bash `shell` tool through
  `tools()`. Commands retain stdout/stderr/exit status, cwd, timeout, and cancellation
  behavior from the original agent.
- **`project.js`**: context-only; reads root `AGENTS.md` (up to 8 KiB), contributes a
  shallow root directory overview (up to 80 entries, bounded displayed text, with
  common dependency/build folders excluded), and detects command candidates from
  `Cargo.toml`, `package.json`, `pyproject.toml`, and `go.mod`. JavaScript scripts are
  selected from build/test/lint/typecheck using npm, pnpm, yarn, or bun lockfiles.
  Detection is heuristic: commands are never executed or verified. Missing files
  are normal; malformed package JSON, truncated inputs, or read failures produce notes.
  Instructions are root-only: no parent traversal or nested instruction discovery.
  All project-specific conventions live in JavaScript, not Rust.

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

### Host capabilities: `host.readFile(options)` / `host.listDir(options)`

Available during `execute()` and `context()`, but not script initialization or
`tools()`. Both operate relative to `ctx.cwd`, reject absolute/parent paths, and do
not follow symlinks. Unknown options are rejected. A shared limit of 64 filesystem
calls applies per invocation; cancellation is checked before each call.

```js
host.readFile({ path: "AGENTS.md", maxBytes: 8192 });
// { text: string, truncated: boolean }, or null if missing
host.listDir({ path: ".", maxEntries: 80 });
// { entries: [{ name, kind: "file" | "directory" | "other" }], truncated: boolean }
```

`readFile` requires `path`, defaults to 16 KiB and permits 1–65536 `maxBytes`.
Only regular files are read; bytes are decoded as UTF-8 with replacement for
invalid/incomplete sequences. `listDir` defaults to `path: "."` and 100 entries;
`maxEntries` must be 1–200. It scans at most 4096 entries, sorts the scanned subset
by filename, then limits the result. Very large directories can produce a different
subset across calls; `truncated` indicates incomplete results. Symlinks are listed
as `other`, never traversed. These are bounded convenience APIs, not a race-proof
filesystem sandbox.

## Trust and prototype limits

**Only load trusted plugins and use a trusted/disposable workspace.** Plugins can
execute arbitrary programs immediately with your user permissions and inherited
environment, including credentials. Loading plugins from a project directory is
not safe for untrusted repositories. There is no approval prompt, filesystem or
network sandbox, or per-plugin permission manifest. Context-time process execution
is disallowed and filesystem helpers reject symlinks/path escapes, but these are
not a complete sandbox; tool-time process execution still grants unrestricted
access. Boa context isolation separates plugin state; it is **not** a security boundary.

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
and provider-error retries. Context tests cover optional entrypoints, registration
once, refresh after tool calls/follow-ups without history pollution, composition,
validation/limits, read-only context execution, bounded filesystem helpers, symlink
rejection, and project discovery including empty/malformed/truncated workspaces.
Authenticated live inference has not been tested.

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
