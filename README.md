# AJ Coding Agent

AJ is an educational (largely for me!) AI-driven agent for software
engineering. Initially inspired by and based on [How to Build an
Agent](https://ampcode.com/how-to-build-an-agent).

Built on the premise that better models just need better tools. We therefore
have a minimal agent loop and focus on providing the right set of builtin
tools, with otherwise minimal scaffolding around it.

## Install

Build and install from source with `cargo`:

```bash
git clone git@github.com:aljoscha/aj.git
cd aj
python3 scripts/bootstrap-codex-code-mode-v8.py
cargo install --path src/aj
```

The experimental Code Mode build requires Python 3.11+, curl, and a native
Linux GNU or macOS toolchain. The bootstrap verifies pinned V8 downloads.
See [Code Mode](docs/code-mode.md) for configuration and build details.

## Authentication

AJ talks to Anthropic and OpenAI models, and you can authenticate either way.

- **Subscription login (OAuth).** With a Claude Pro/Max or ChatGPT Plus/Pro
  plan, open the command palette (`Ctrl+O`) and choose **login**. Credentials
  are stored in `~/.aj/auth.json`. You can also provide a token directly via
  `ANTHROPIC_OAUTH_TOKEN` or `OPENAI_CODEX_OAUTH_TOKEN`.
- **API key.** Put an `ANTHROPIC_API_KEY` or `OPENAI_API_KEY` in a `.env` file,
  either in the working directory or a global one at `~/.aj/.env`. You can also
  export it in your environment.

## Quickstart

```bash
aj                          # start an interactive session in the current project
aj "explain this codebase"  # submit a first message on launch
```

## Using AJ

Most actions live in the **command palette**, opened with
`Ctrl+O`. From there you can switch model, set the reasoning effort, start or
resume a session, log in or out, check usage across providers, toggle skills,
set or manage a **Goal**, open settings, and more.

A handful of keys worth knowing:

| Key | Action |
| --- | --- |
| `Enter` | Send your message |
| `Ctrl+J` | Insert a newline (`Shift+Enter` also works where supported) |
| `Ctrl+O` | Open the command palette |
| `Ctrl+R` | Search your prompt history |
| `Tab` | Focus the transcript, then step through older user messages |
| `Shift+Tab` | Step back toward newer user messages |
| `b` | Branch from the focused message |
| `Ctrl+C` | Interrupt the current turn (press again when idle to quit) |

## Sessions

Conversations are saved as resumable sessions, scoped to the project directory
you run `aj` in:

```bash
aj list-sessions       # list this project's sessions
aj continue            # resume the most recent session that is not archived
aj continue <id>       # resume a specific session, archived or not
```

You can also resume a session or start a fresh one from the command palette.

The resume picker and prompt-history search (`Ctrl+R`) match case-insensitive
literal terms. `editor footer` requires both substrings, in any order, and
`"editor footer"` requires a contiguous phrase. Contiguous matches rank before
scattered terms. Resume searches the full opening prompt, tag, session ID, and
host label. Prompt history searches the full prompt, even beyond its preview.
Other pickers, including the command palette, use fuzzy matching.

Press `Tab` to focus the transcript and move through earlier user messages.
Press `b` on a focused message to edit it and continue from that point,
creating a new branch while preserving the existing conversation. Open
**session tree** from the command palette to view the branches in the current
session and switch between them.

## Print mode

For one-shot and scripted use, `--print` runs a single turn, streams to stdout,
and exits:

```bash
aj --print "summarize the build setup"      # final answer as plain text
aj --print --format json "..."              # one JSON event per line (JSONL)
```

`--format json` emits the event stream as JSONL for piping into other tools. It
requires `--print`.

## Feature highlights

- **Minimal system prompt.** AJ keeps its built-in prompt deliberately small.
  Replace it with `~/.agents/SYSTEM_PROMPT.md` or
  `~/.claude/SYSTEM_PROMPT.md`. Project and user guidance files are layered on
  top.
- **Skills.** AJ supports skills, discovered from the usual `skills/`
  directories under `.aj/`, `.agents/`, and `.claude/` in your project or home
  directory. Enable or disable them in the config.
- **Queue and steer.** While AJ is working, `Enter` queues a follow-up for when
  the turn finishes, and `Alt+Enter` steers by injecting your message into the
  running turn at the next step.
- **Sub-agents.** AJ can spawn sub-agents and run shell commands or sub-agents
  in the background. Open the agent view (`Alt+A`) to switch between them and
  follow or stop background work.
- **Images.** @-mention an image (or any file) in your message, or paste one
  from the clipboard with `Ctrl+V`, and AJ can read it.

## Configuration

Configuration lives in `~/.aj/config.toml`. The settings window (open it from
the command palette) covers every option and writes your changes there. You can
also edit the file by hand.

Set `auto_compact_during_turn = true` to let the main agent compact context
within a running turn in interactive and print mode. It defaults to `false`
and requires `auto_compact = true` (the default). After each full tool batch,
before the next inference, AJ checks provider-reported input occupancy against
`compact_threshold` (default `0.85` of the context window). It never interrupts
a streaming response. Changes made in settings take effect at the next
compaction check, even during a running turn. A compaction already underway
keeps its settings. Compaction uses the usual compaction flow and the same
run stays active. If compaction fails, the original history is preserved and
continuation stops, including overflow recovery, with no automatic retry after
the failure. Queued messages and task notices wait until you explicitly start
work again. The failure remains in the transcript after
reconnecting or reopening the session, without becoming model input.
Post-turn threshold triggering is unchanged: interactive mode
checks the threshold, while print mode does not.

`transcript_mode = "full"` selects transcript detail. The settings window cycles
through `full` (the default), `compact`, and `focused`, applying changes immediately.
This controls display, not context-window compaction. The legacy
`compact_transcript` boolean is accepted when `transcript_mode` is absent in the
same file, mapping `true` to `compact` and `false` to `full`. Project config still
overrides user config, regardless of which spelling each file uses.

Full shows individual activity entries with their usual previews. Compact keeps
tool headers and bash commands. Focused folds activity between messages into
muted summaries such as `thinking ×2 · read_file ×3 · bash ×2`, with running work
and failures called out. Background task notifications join these groups as
`task results ×N`, with failed and stopped outcomes visible in the summary.
Assistant text and ordinary notices remain visible outside the groups.

Click an activity summary to expand or fold that group. Text inside an expanded
group remains selectable. For keyboard access, focus the transcript with `Tab`,
use `[` / `]` to select the previous / next activity group, and press `Enter` to
toggle it. The tools-expand action also expands all activity groups. Individual
fold choices last for the current view session and do not change the saved log.

### Keybindings

Override an action's shortcut in the `[keybindings]` table:

```toml
[keybindings]
"aj.palette.open" = "ctrl+shift+p"
```

Overrides accept the full chord grammar, including `ctrl+shift+p`, `shift+enter`,
and function keys through `f35`. Some chords require the Kitty keyboard protocol
and may not work, or may arrive as a different key, on other terminals. Built-in
action shortcuts remain portable to terminals using legacy input encodings.
Invalid syntax, unknown actions, reserved keys, and conflicting assignments are
rejected with a startup warning. Actions can trade shortcuts when the final
assignments are conflict-free.

## Contributing

AJ is a Cargo workspace. See [`CLAUDE.md`](CLAUDE.md) for build/test commands,
the crate layout, and code-style conventions.

## License

[MIT](LICENSE)
