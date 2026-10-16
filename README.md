# ngrc

A minimal Rust coding agent built with the Goose GDK. An interactive chat with
one in-memory conversation and exactly one tool: `shell`. Nothing saves the
conversation to disk; it is discarded when the process exits.

The GDK handles inference, tool dispatch, and the model/tool loop. This application
supplies an in-memory session, system prompt, and a `/bin/bash -c` tool. The initial
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

Commands start in the process's current directory. Each shell call is independent:
`cd` and shell variables do not persist. stdout, stderr, and exit status are returned
to the model; commands are logged to stderr and assistant text streams to stdout.
The chat has a simple line prompt, but shell commands have no interactive terminal.
No saved sessions or other tools are included.

**Warning:** commands execute immediately, with your user permissions and inherited
environment (including credentials). There is no approval prompt, filesystem/network
sandbox, or restriction to the current directory. Use only in a trusted/disposable
workspace. Working directory selection is not a security boundary.

Ctrl-C cancels the agent. Each shell command has a 120-second timeout and the direct
shell process is killed when dropped, but descendant processes are not reliably
terminated. Output is buffered without a size cap. There is no model-turn/cost limit
or context compaction. These are deliberate prototype limitations, not production
safety guarantees.

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
and retrying after provider errors. No paid/live model call is required for tests.
Authenticated live inference has not been tested.

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
  enum and intentionally discards usage accounting.
- Provider failures can be represented as error messages in a successful machine
  result. The CLI renders these and returns to the prompt for a follow-up/retry.
- Each turn builds a fresh machine around the same in-memory session. The complete
  conversation stays available to inference without disk persistence.
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

Pinned component API references (source, not assumptions about the Goose CLI):

- [`goose-agent` README](https://github.com/aaif-goose/goose/blob/4811ae25f736fdf0ede847449c060e96645426b5/crates/goose-agent/README.md)
- [`goose-agent` inference](https://github.com/aaif-goose/goose/blob/4811ae25f736fdf0ede847449c060e96645426b5/crates/goose-agent/src/inference.rs)
- [`goose-agent` tools](https://github.com/aaif-goose/goose/blob/4811ae25f736fdf0ede847449c060e96645426b5/crates/goose-agent/src/tool.rs)
- [`goose-providers` OpenAI](https://github.com/aaif-goose/goose/blob/4811ae25f736fdf0ede847449c060e96645426b5/crates/goose-providers/src/openai.rs)

`NGRC_MODEL` and the commands above belong to this project, not the Goose CLI.
