// A plugin is a plain script whose final expression is its API object.
// No Node.js runtime, imports, or Rust-side registration is required.
({
  apiVersion: 1,
  tools: [
    {
      name: "shell",
      description:
        "Execute a non-interactive Bash command in the project directory. Use this to inspect, edit, and test code. Commands time out after 120 seconds; cwd changes do not persist between calls.",
      inputSchema: {
        type: "object",
        properties: {
          command: { type: "string", description: "A non-interactive Bash command." },
        },
        required: ["command"],
        additionalProperties: false,
      },
      execute(input, context) {
        return host.exec({
          program: "/bin/bash",
          args: ["-c", input.command],
          timeoutMs: 120000,
        });
      },
    },
  ],
})
