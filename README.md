# AJ Coding Agent

AJ is an educational (largely for me!) AI-driven agent for software
engineering. Initially inspired by and based on [How to Build an
Agent](https://ampcode.com/how-to-build-an-agent).

Built on the premise that better models just need better tools. We therefore
have a minimal agent loop and focus on providing the right set of builtin
tools, with otherwise minimal scaffolding around it.

## Installation & Setup

Install using `cargo`:

```bash
$ cargo install --path src/aj
```

You will need a `.env` file with an `ANTHROPIC_API_KEY` (or
`OPENAI_API_KEY`) either in the working directory or a global one in
`~/.aj/`.

## Running

The `aj` binary serves both an interactive TUI session and a non-interactive
print mode through the same code path:

```bash
$ aj                            # interactive TUI (default)
$ aj "draft a release note"     # prefill the first turn, then TUI
$ aj --print "explain this"     # one-shot, stream to stdout, exit
$ aj --print --format json …    # same, but JSONL events per line
```

Non-conversational subcommands short-circuit before mode dispatch:

```bash
$ aj list-threads               # threads for the current project
$ aj continue                   # resume the most recent thread
$ aj continue <thread-id>       # resume a specific thread
$ aj models update              # refresh ~/.aj/models.json from models.dev
```

Inside the TUI, slash commands open inline selectors:

| Command | Effect |
|---|---|
| `/model` | Pick the active model (fuzzy filter; persists as default). |
| `/thinking [level]` | Set the reasoning budget (off / low / medium / high / xhigh / max). |
| `/session` | Resume a different thread for this project. |
| `/clear` | Start a fresh thread (the current one stays on disk). |
| `/help` | Show the slash-command reference. |
| `/quit` | Exit. |

## Configuration

Persistent state lives under `~/.aj/`:

- `.env` — secrets (API keys), loaded before any project-local `.env`.
- `config.toml` — defaults (model, thinking level, speed, theme, disabled
  tools). CLI flags and env vars override.
- `models.json` — model catalog consumed by the `/model` selector.
  Refreshed by `aj models update`.
- `themes/<name>.json` — optional user-supplied themes layered on top of
  the bundled `dark` / `light` palettes. The active theme hot-reloads on
  file changes.
- `threads/<project>/` — JSONL conversation logs, one file per thread.

Model selection precedence (highest to lowest): CLI flags
(`--model-api`, `--model-url`, `--model-name`) → env vars (`MODEL_API`,
`MODEL_URL`, `MODEL_NAME`) → `config.toml` → built-in defaults.

## Crate layout

The workspace splits along the dependency graph from
[`docs/aj-next-plan.md`](docs/aj-next-plan.md):

```
aj-models  ←  aj-agent  ←  aj-tools
                ↑              ↑
                └─  aj-session  ─┘
                        ↑
                        aj
```

- `aj-models` — wire layer: provider SDKs, unified `Message` /
  `AssistantMessage` / streaming types, model registry.
- `aj-agent` — the `Agent` runtime, the typed `AgentEvent` bus, the
  tool trait, and `ToolDetails` for structured tool rendering.
- `aj-session` — on-disk thread format, `ConversationLog`, replay.
- `aj-tools` — the builtin tool implementations
  (`read_file`, `bash`, `write_file`, `edit_file`,
  `edit_file_multi`, `agent`, `todo_read`, `todo_write`).
- `aj-tui` — in-process text-UI framework (layout, components, theming).
- `aj-conf` — `~/.aj/config.toml` loader and path helpers.
- `aj` — the binary: CLI parsing, print mode, interactive TUI, slash
  commands, selectors.
- `anthropic-sdk` / `openai-sdk` — thin async clients used by
  `aj-models`'s provider adapters.
