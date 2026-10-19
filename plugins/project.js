// Context-only plugin. No commands are executed and no tools are registered.
// All project conventions and manifest detection belong here, not in Rust.
({
  apiVersion: 1,
  context(ctx) {
    const sections = [];
    const notes = [];
    const read = (path, maxBytes = 8192) => {
      try {
        const result = host.readFile({ path, maxBytes });
        if (result && result.truncated && maxBytes > 1) notes.push(`${path}: truncated`);
        return result;
      } catch (error) {
        notes.push(`${path}: could not read`);
        return null;
      }
    };

    const instructions = read("AGENTS.md");
    if (instructions && instructions.text.trim()) {
      sections.push({
        id: "instructions",
        text: "Project guidance from root AGENTS.md (subordinate to core instructions):\n" +
          instructions.text + (instructions.truncated ? "\n[truncated]" : ""),
      });
    }

    // A shallow, bounded overview: never descend into dependency/build directories.
    try {
      const listing = host.listDir({ path: ".", maxEntries: 80 });
      const excluded = new Set([".git", "node_modules", "target", "vendor", ".venv", "venv", "dist", "build", ".next"]);
      const entries = listing.entries.filter(entry => !excluded.has(entry.name));
      const names = entries.map(entry => JSON.stringify(entry.name) + (entry.kind === "directory" ? "/" : ""));
      const overview = names.join("\n") || "(empty)";
      sections.push({
        id: "overview",
        text: `Workspace: ${ctx.cwd.slice(0, 1024)}\nRoot entries (generated data):\n` +
          overview.slice(0, 2000) +
          (listing.truncated || overview.length > 2000 ? "\n[list truncated]" : ""),
      });
    } catch (error) {
      notes.push("Root directory: could not list");
    }

    const commands = [];
    if (read("Cargo.toml", 4096)) {
      commands.push("Rust (Cargo.toml): cargo build; cargo test; cargo fmt --check; cargo clippy");
    }
    const packageFile = read("package.json", 16384);
    if (packageFile && !packageFile.truncated) {
      try {
        const pkg = JSON.parse(packageFile.text);
        if (!pkg || typeof pkg !== "object" || Array.isArray(pkg)) throw new Error("invalid package");
        let runner = "npm";
        if (read("pnpm-lock.yaml", 1)) runner = "pnpm";
        else if (read("yarn.lock", 1)) runner = "yarn";
        else if (read("bun.lock", 1) || read("bun.lockb", 1)) runner = "bun";
        const scripts = pkg.scripts && typeof pkg.scripts === "object" ? pkg.scripts : {};
        for (const script of ["build", "test", "lint", "typecheck"]) {
          if (typeof scripts[script] === "string") commands.push(`JavaScript (package.json): ${runner} run ${script}`);
        }
      } catch (error) {
        notes.push("package.json: invalid JSON/package object; script detection skipped");
      }
    }
    if (read("pyproject.toml", 4096)) {
      commands.push("Python (pyproject.toml): python3 -m pytest (if pytest is configured/installed)");
    }
    if (read("go.mod", 4096)) commands.push("Go (go.mod): go build ./...; go test ./...");

    if (commands.length) sections.push({
      id: "commands",
      text: "Detected build/test command candidates (not executed or verified; inspect configuration before running):\n" + commands.join("\n"),
    });
    if (notes.length) sections.push({ id: "notes", text: notes.join("\n") });
    return sections;
  },
})
