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
cargo install --path src/aj
```

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
open settings, and more.

A handful of keys worth knowing:

| Key | Action |
| --- | --- |
| `Enter` | Send your message |
| `Shift+Enter` | Insert a newline |
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

## Contributing

AJ is a Cargo workspace. See [`CLAUDE.md`](CLAUDE.md) for build/test commands,
the crate layout, and code-style conventions.

## License

[MIT](LICENSE)
