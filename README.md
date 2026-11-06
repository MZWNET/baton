# baton

Keep the conversations of your coding agents in one append-only history, back it up or sync it anywhere, and continue any of them in another agent.

baton reads and writes sessions of [pi](https://github.com/earendil-works/pi), [Oh My Pi](https://github.com/can1357/oh-my-pi) (`omp`), [Claude Code](https://github.com/anthropics/claude-code) (`claude`), [Codex](https://github.com/openai/codex), [OpenCode](https://github.com/anomalyco/opencode), [Kilo Code](https://github.com/Kilo-Org/kilocode) (`kilo`), [Hermes Agent](https://github.com/NousResearch/hermes-agent) (`hermes`), [MastraCode](https://github.com/mastra-ai/mastra/tree/main/mastracode), [Devin CLI](https://docs.devin.ai/cli) (`devin`), [GitHub Copilot CLI](https://github.com/github/copilot-cli) (`copilot`), [Grok Build](https://github.com/xai-org/grok-build) (`grok`), [OpenHands CLI](https://github.com/OpenHands/OpenHands-CLI) (`openhands`), [Mistral Vibe](https://github.com/mistralai/mistral-vibe) (`vibe`), [Qwen Code](https://github.com/QwenLM/qwen-code) (`qwen`), [Kimi Code](https://github.com/MoonshotAI/kimi-code) (`kimi`), [Muse Code](https://dev.meta.ai/docs/muse-code/) (`muse`), [Antigravity CLI](https://antigravity.google/docs/cli/) (`antigravity`) and [Cursor Agent](https://cursor.com/cli) (`cursor`). It also reads, but cannot write, the [Antigravity](https://antigravity.google/) desktop app and IDE (`antigravity-desktop`, `antigravity-ide`) and the [Cursor](https://cursor.com/) editor (`cursor-ide`). It runs on Linux, macOS and Windows.

## Usage

Tagged versions are published on [Releases](https://github.com/MZWNET/baton/releases). For a preview of the latest code, download your platform's archive from the artifacts of the newest [Build](https://github.com/MZWNET/baton/actions/workflows/build.yml?query=branch%3Amaster) run. With Nix, `nix profile add github:MZWNET/baton` installs the latest code, and the flake's `overlays.default` adds `pkgs.baton`.

```sh
cargo install --path crates/baton-cli

baton import                       # read new and changed sessions of every agent
baton list [--cwd] [--agent omp]
baton search <terms>
baton show <id>
baton export <id> --to claude      # write a new Claude Code session and print its id
baton resume <id> --in codex       # export, then open it in Codex
```

`<id>` is a baton session id, a unique prefix of it, or `agent:native_id` such as `omp:01a1a978`.

Imports are incremental and idempotent. A session exported to another agent and continued there flows back into the same baton session on the next import; when the baton session has moved on meanwhile, the continuation becomes a fork that records where it branched.

## Storage

```
<data>/sessions/<id>/events.jsonl   append-only events
<data>/sessions/<id>/meta.json      title, cwd, times and links to native sessions
<data>/blobs/<sha256>               images
```

`<data>` is `BATON_HOME`, or the platform data directory (`~/.local/share/baton` on Linux, `~/Library/Application Support/baton` on macOS, `%APPDATA%\baton\data` on Windows). Apart from `meta.json`, files are only ever added or appended to, so the directory suits incremental sync tools. Import cursors and the search index live in `BATON_CACHE_DIR` or the platform cache directory, and are rebuilt when missing.

## Worth knowing

- Agent sessions are only read during import. Exports create new sessions and never overwrite existing ones.
- Thinking is stored but never written back to an agent, tool calls are written as history and never run again, and auth, hooks and MCP settings are never touched.
- Each agent's own location variables are honored, such as `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `XDG_DATA_HOME` and `XDG_CONFIG_HOME`.
- OpenCode and Kilo Code sessions go through their `opencode` and `kilo` commands, so those have to be on `PATH`.

## Credits

The session format research builds on [session-migrate](https://github.com/xhluca/session-migrate) and [omnisession](https://github.com/bvolpato/omnisession).

## License

[MIT](LICENSE)
