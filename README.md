# ngrc

A minimal Rust coding agent built with the Goose GDK. An interactive chat with
one persistent conversation and exactly one tool: `shell`. Each session is stored
in an append-only `.jsonl` file, including its metadata.

The GDK handles inference, tool dispatch, and the model/tool loop. This application
supplies a file-backed session, system prompt, and a `/bin/bash -c` tool. The initial
backend is OpenAI; there is no Goose CLI configuration dependency or MCP server
process. Requires Rust 1.94.1+ and `/bin/bash` (macOS/Linux).

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
No other tools are included.

**Warning:** commands execute immediately, with your user permissions and inherited
environment (including credentials). There is no approval prompt, filesystem/network
sandbox, or restriction to the current directory. Use only in a trusted/disposable
workspace. Working directory selection is not a security boundary.

Ctrl-C cancels the agent. Each shell command has a 120-second timeout and the direct
shell process is killed when dropped, but descendant processes are not reliably
terminated. Output is buffered without a size cap. There is no model-turn/cost limit
or context compaction. These are deliberate prototype limitations, not production
safety guarantees.

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

Tests use fake providers and a real local shell: only the shell schema is exposed,
stdout/stderr/exit status and cwd are preserved, Bash-specific syntax works, and a
model → shell → model turn finishes. Chat tests cover follow-up context including
prior shell results, optional initial input, blank lines, EOF, idle cancellation,
and retrying after provider errors. Storage tests cover byte-prefix preservation,
metadata and usage round trips, message-delta replay, exclusive locks, failed
writes, corrupt/incomplete logs, and resuming a model → shell → model conversation.
No paid/live model call is required for tests. Authenticated live inference has
not been tested.

## GDK notes / gaps

Pinned to the published `0.1.0-alpha.12` versions of `goose-agent`,
`goose-providers`, and `goose-provider-types`, with `Cargo.lock` checked into the
project. `goose-providers` explicitly enables `rustls-tls` for HTTPS.

No missing GDK feature blocked this minimal implementation. A few assembly details
are worth recording rather than building a framework around them:

- The current orchestration API is `StateMachine`, not a high-level `Agent` builder.
  The caller implements `MachineSession`, `SessionLoader`, and `EffectHandler`.
- `InferenceRunner` needs an effect type implementing `InferenceEffect`; the built-in
  `ConversationEffect` does not implement it. This app uses a small custom effect
  enum and persists usage effects alongside conversation effects.
- Provider failures can be represented as error messages in a successful machine
  result. The CLI renders these and returns to the prompt for a follow-up/retry.
- Each turn builds a fresh machine around the same file-backed session. In-process
  loads clone the persisted in-memory view; reopening replays the append-only log.
- Tool approval, shell isolation, bounded output, process-tree cleanup, and a run
  budget are not supplied by this minimal assembly. They remain unimplemented here;
  this is not a claim that no other GDK component can help with them.
- The official documentation map did not list GDK component API documentation.
  The crate APIs were verified against the pinned published source and compile/tests.

### Verification Completed — references

Official docs consulted for provider/tool context:

- [Configure LLM Provider](https://goose-docs.ai/docs/getting-started/providers)
  — OpenAI and `OPENAI_API_KEY`.
- [Using Extensions](https://goose-docs.ai/docs/getting-started/using-extensions)
  — Goose's extension/MCP architecture. This project instead registers its shell
  directly in-process through the GDK.

Session lifecycle context was checked against
[Session Management](https://goose-docs.ai/docs/guides/sessions/session-management).
This project's JSONL schema and session flags are its own, not Goose CLI behavior.

Pinned component API references (source, not assumptions about the Goose CLI):

- [`goose-agent` README](https://github.com/aaif-goose/goose/blob/4811ae25f736fdf0ede847449c060e96645426b5/crates/goose-agent/README.md)
- [`goose-agent` inference](https://github.com/aaif-goose/goose/blob/4811ae25f736fdf0ede847449c060e96645426b5/crates/goose-agent/src/inference.rs)
- [`goose-agent` tools](https://github.com/aaif-goose/goose/blob/4811ae25f736fdf0ede847449c060e96645426b5/crates/goose-agent/src/tool.rs)
- [`goose-providers` OpenAI](https://github.com/aaif-goose/goose/blob/4811ae25f736fdf0ede847449c060e96645426b5/crates/goose-providers/src/openai.rs)

`NGRC_MODEL` and the commands above belong to this project, not the Goose CLI.
