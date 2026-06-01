# Glass — project notes

## Good Software Engineering

Any Rust module over **1500 lines** should be refactored and split into smaller modules. 

Similar code should be factorised out and reused. We should not have multiple functions doing the same thing for different data e.g. instruction sets.

Unit tests should cover most functions. Ideally we should start with a unit test and then prove the function satisfies it.

## Skill catalog parity

`glass-api/src/skills.rs` is the single source of truth for the
automation API surface that drives both the CLI subcommands and the
MCP server (`glass-mcp`). When you add, rename, or remove a verb,
update **all four** in the same change:

1. The `glass_api` function / method that does the work.
2. The CLI subcommand wiring in `glass-cli/src/main.rs` +
   `verbs.rs` (clap derive + dispatch arm + text renderer).
3. The `Skill { … }` entry in `glass-api/src/skills.rs` (name must
   match the CLI subcommand name kebab-cased).
4. The dispatch arm in `glass-mcp/src/dispatch.rs` (matches on the
   same kebab-case name; extracts args from the JSON object).

Also update `docs/cli-api.md` to keep the user-facing reference in
sync. Skipping the catalog or the MCP dispatcher is the failure
mode that bites here — the CLI keeps working while LLM-driven
tooling silently loses the verb.

### MCP-only verbs

A handful of verbs are inherently stateful (bundle-open /
bundle-close / bundle-status, Frida session control). They live in
the MCP dispatcher and the skill catalog but have no CLI semantic
— the CLI is one-shot and re-opens per call. For these:

1. Skill catalog entry: present, marked MCP-only in the description.
2. MCP dispatcher: full implementation.
3. CLI: a stub variant that prints "this verb is MCP-only" and
   exits non-zero, so users who try it get a clear pointer.
4. docs/cli-api.md: a short paragraph listing them.
