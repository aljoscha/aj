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

You will need a `.env` file with an `ANTHROPIC_API_KEY` either in the working
directory or a global one in `~/.config/aj/`.
