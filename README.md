# ngrc

A minimal Rust coding agent built with oi-kit. It provides an interactive chat with
one persistent conversation, a `shell` tool, and optional MCP extensions. Each session is stored
in an append-only `.jsonl` file, including its metadata.

oi-kit handles inference, tool dispatch, and the model/tool loop. This application
supplies a file-backed session, system prompt, and a `/bin/bash -c` tool. The initial
backend is OpenAI; there is no Goose CLI configuration dependency.
Requires Rust 1.95.0+ and `/bin/bash` (macOS/Linux). Cargo downloads oi-kit from
crates.io; no local Goose checkout is needed.

## Running this project

Set `OPENAI_API_KEY` and `NGRC_MODEL` in your environment. The model must support
tool calling; there is deliberately no hardcoded model default. From this directory:

```sh
cargo run --locked
# Or send an initial message before entering the same chat:
cargo run --locked -- "Inspect this project and summarize it without changing files"
```

With no arguments, the app starts at a `>` prompt without making a model request.
Type a message and press Enter; after the reply, send follow-up messages at the
next prompt. Earlier messages, replies, and shell results remain in context.
Blank lines are ignored. EOF (Ctrl-D on an empty line) exits cleanly; Ctrl-C exits
whether waiting for input or running a turn. Each input line is a single message.
Provider errors are displayed and return to the prompt so you can retry.

Commands start in the session's saved working directory (the process's current
directory when the session was created). Each shell call is independent:
`cd` and shell variables do not persist. stdout, stderr, and exit status are returned
to the model; commands are logged to stderr and assistant text streams to stdout.
The chat has a simple line prompt, but shell commands have no interactive terminal.
No other tools are included unless you explicitly load MCP extensions.

**Warning:** shell commands and MCP tools execute immediately, with your user permissions and inherited
environment (including credentials). There is no approval prompt, filesystem/network
sandbox, or restriction to the current directory. Use only in a trusted/disposable
workspace. Working directory selection is not a security boundary.

Ctrl-C cancels the agent. Each shell command has a 120-second timeout and the direct
shell process is killed when dropped, but descendant processes are not reliably
terminated. Output is buffered without a size cap. There is no model-turn/cost limit
or context compaction. These are deliberate prototype limitations, not production
safety guarantees.

## MCP extensions (MVP)

Pass `--extensions FILE` before the optional prompt to load a JSON array of oi-kit
transport configurations. Nothing is auto-discovered, installed, or enabled.
For example, a file can contain:

```json
[
  {
    "type": "stdio",
    "name": "local",
    "cmd": "/absolute/path/to/mcp-server",
    "args": [],
    "envs": {},
    "timeout": 120
  },
  {
    "type": "streamable_http",
    "name": "remote",
    "uri": "http://localhost:8000/mcp",
    "timeout": 120
  }
]
```

Only include servers you trust. Stdio commands are executed directly, not through
Bash; their default working directory is the saved session cwd. An optional `cwd`
must be absolute. Processes inherit the CLI environment; `envs` supplies overrides
subject to oi-kit filtering. HTTP `headers` can supply authentication; this MVP has no
OAuth login flow or secret store. Do not commit credentials.

Servers start at launch; an initialization failure stops the CLI with an error.
The same connections are reused across chat turns and released on exit. Tools are
namespaced (for example, `local__search`); server instructions join the system
prompt. All tools run without confirmation, just like shell. There is no management
UI, hot reload, or automatic package installation.

Configuration is launch-scoped, not saved in the session: pass `--extensions` again
when resuming. Tool calls and results are saved in the JSONL log.

## Session storage

Every launch creates a new `<uuid>.jsonl` session and prints its path to stderr.
The default directory is `~/.ngrc/sessions`; `NGRC_SESSION_DIR` overrides it, and
`--session-dir DIR` takes precedence over both. `--resume FILE.jsonl` opens an
existing session instead of creating one. These options go before the optional
prompt; `--` separates options from a prompt beginning with a dash.

Resuming restores conversation history, shell results, usage, and the saved working
directory. It waits for input unless an initial prompt is supplied; it does not
rerun old shell commands. Both `OPENAI_API_KEY` and `NGRC_MODEL` are still required.
Changing models on resume appends a metadata record; it never edits the header.
`--session-dir` and `--resume` cannot be combined.

The versioned log contains one JSON object per line, with a `type` field:

- `created`: format `version`, session `id`, Unix-seconds `created_at`, and
  `metadata` (`cwd`, `provider`, `model`). This is always the first record.
- `metadata`: the new complete `metadata` value, applied from that point onward.
- `message`: a `message` containing user input, an assistant response, a tool
  request/result, or a message metadata update.
- `usage`: a provider `usage` record, preserved rather than discarded.

The app only appends: it never rewrites, truncates, or compacts a session, and has
no mutable metadata sidecar or database. Replaying records reconstructs the current
state, including message-delta and metadata-only updates. User messages are saved
before inference; assistant/tool/usage effects are saved when the agent applies
them, not on every streamed token. Appends are synced before updating memory.
A crash can therefore lose uncommitted streamed output or leave an incomplete final
record. Invalid, unsupported-version, and incomplete logs are rejected without
modifying them; automatic crash-tail repair is not implemented.

An exclusive advisory lock prevents simultaneous app writers. New session files
have owner-only permissions (`0600` on Unix). Logs contain plaintext prompts,
responses, and shell output, which may include secrets; protect them accordingly.
Append-only is an application behavior, not tamper protection: the unsandboxed
shell (or any external program with permissions) can still modify these files.

## Verification

```sh
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

Tests use fake providers and a real local shell: without extensions only the shell schema is exposed,
stdout/stderr/exit status and cwd are preserved, Bash-specific syntax works, and a
model → shell → model turn finishes. Chat tests cover follow-up context including
prior shell results, optional initial input, blank lines, EOF, idle cancellation,
and retrying after provider errors. Storage tests cover byte-prefix preservation,
metadata and usage round trips, message-delta replay, exclusive locks, failed
writes, corrupt/incomplete logs, and resuming a model → shell → model conversation.
MCP tests use a local stdio fixture and require `python3`. No paid/live model call
or remote MCP server is required. Authenticated live inference has not been tested.

## oi-kit notes

The application uses [`oi-kit-agent`](https://crates.io/crates/oi-kit-agent),
[`oi-kit-provider-types`](https://crates.io/crates/oi-kit-provider-types), and
[`oi-kit-providers`](https://crates.io/crates/oi-kit-providers) from crates.io.
It uses `rmcp` 3.9.0 and rustls for provider and agent HTTPS.

The application assembles `StateMachine` with the built-in shell,
`ExtensionOperation`, unhandled-call handling, and inference.
`ExtensionManager` owns MCP transport setup, tool catalogs, dispatch, and connection
lifetimes. The application only supplies a minimal host and launch-scoped source.
The [oi-kit-agent API docs](https://docs.rs/oi-kit-agent) and this project's tests
are the reference for the MCP APIs used here.
