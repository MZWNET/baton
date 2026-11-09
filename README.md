# baton

Keep the conversations of your coding agents in one append-only history, back it up or sync it anywhere, and continue any of them in another agent. Runs on Linux, macOS, and Windows.

## Supported agents

| Agent | `--agent` / `--to` / `--in` | Read / Write |
| --- | --- | --- |
| [pi](https://github.com/earendil-works/pi) | `pi` | Read / Write |
| [Oh My Pi](https://github.com/can1357/oh-my-pi) | `omp` | Read / Write |
| [Claude Code](https://github.com/anthropics/claude-code) | `claude` | Read / Write |
| [Codex](https://github.com/openai/codex) | `codex` | Read / Write |
| [OpenCode](https://github.com/anomalyco/opencode) | `opencode` | Read / Write |
| [Kilo Code](https://github.com/Kilo-Org/kilocode) | `kilo` | Read / Write |
| [Hermes Agent](https://github.com/NousResearch/hermes-agent) | `hermes` | Read / Write |
| [MastraCode](https://github.com/mastra-ai/mastra/tree/main/mastracode) | `mastracode` | Read / Write |
| [Devin CLI](https://docs.devin.ai/cli) | `devin` | Read / Write |
| [GitHub Copilot CLI](https://github.com/github/copilot-cli) | `copilot` | Read / Write |
| [Grok Build](https://github.com/xai-org/grok-build) | `grok` | Read / Write |
| [OpenHands CLI](https://github.com/OpenHands/OpenHands-CLI) | `openhands` | Read / Write |
| [Mistral Vibe](https://github.com/mistralai/mistral-vibe) | `vibe` | Read / Write |
| [Qwen Code](https://github.com/QwenLM/qwen-code) | `qwen` | Read / Write |
| [Kimi Code](https://github.com/MoonshotAI/kimi-code) | `kimi` | Read / Write |
| [Muse Code](https://dev.meta.ai/docs/muse-code/) | `muse` | Read / Write |
| [Antigravity CLI](https://antigravity.google/docs/cli/) | `antigravity` | Read / Write |
| [Cursor Agent](https://cursor.com/cli) | `cursor` | Read / Write |
| [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness) | `dsh` | Read / Write |
| [Antigravity](https://antigravity.google/) desktop app | `antigravity-desktop` | Read only |
| [Antigravity](https://antigravity.google/) IDE | `antigravity-ide` | Read only |
| [Cursor](https://cursor.com/) editor | `cursor-ide` | Read only |
| [Paseo](https://github.com/getpaseo/paseo) | `paseo` | Read / Write |

## Install

- Tagged versions are on [Releases](https://github.com/MZWNET/baton/releases).
- Preview builds are in the artifacts of the newest [Build](https://github.com/MZWNET/baton/actions/workflows/build.yml?query=branch%3Amaster) workflow run.
- Nix: `nix profile add github:MZWNET/baton`, or use the flake's `overlays.default` (`pkgs.baton`).
- From source: `cargo install --path crates/baton-cli`.

## Usage

```sh
baton import [--agent omp] [--path <path>]      # read new and changed sessions
baton list [--cwd [<dir>]] [--agent omp]
baton show <id>
baton export <id> --to claude [--cwd <dir>]     # write a new Claude Code session and print its id
baton export <id> --to paseo --provider claude  # write it for Claude Code, then register it in Paseo
baton resume <id> --in codex [--cwd <dir>]      # export, then open it in Codex
baton materialize --in omp --in claude          # write every session into each agent, in the matching local project
baton migrate                                   # convert a data directory from the old events.jsonl and meta.json layout
```

`<id>` is a baton session id, a unique prefix of it, or `agent:native_id` such as `omp:01a1a978`.

### Syncing and continuing

- Imports are incremental and idempotent.
- A session exported to another agent and continued there flows back into the same baton session on the next import, becoming a fork that records where it branched if the baton session moved on meanwhile.
- `baton materialize --in <agent>` (repeatable) imports that agent first, then writes every baton session it lacks on this machine as a native session so the agent's resume list shows them; rerunning it writes nothing new.
- When a session moves on after its materialized copy was written, baton writes a fresh copy and deletes the old one only if baton wrote it and it was never continued; Codex copies listed in `session_index.jsonl` and OpenHands keep the old copy with a warning.

### Project directories

`export`, `resume`, `materialize`, and `list --cwd` locate a session's project on this machine by git identity (the `origin` remote, or else the root commit, plus the directory inside the repository), falling back to path mapping and then the recorded directory:

| Variable | Purpose |
| --- | --- |
| `BATON_PROJECTS` | `PATH`-separated directories searched up to three levels deep for matching git checkouts |
| `BATON_PATH_MAP` | `PATH`-separated `from=to` path prefix pairs used when no checkout matches (for example `/root/repos=/Users/me/repos`) |

- `list --cwd` also matches sessions recorded under another machine's path of the same repository.
- OpenCode and Kilo Code require the project directory to exist to receive a session.

## Storage

```
<data>/sessions/<id>/events/<host>-<uuid>.jsonl.zst   zstd-compressed events
<data>/sessions/<id>/records/<host>-<uuid>.json       title, cwd, fork point or a link to a native session
<data>/blobs/<sha256>                                 images
```

| Setting | Default |
| --- | --- |
| `BATON_HOME` (`<data>`) | `~/.local/share/baton` (Linux), `~/Library/Application Support/baton` (macOS), `%APPDATA%\baton\data` (Windows) |
| `BATON_HOST` (`<host>`) | Machine host name |
| `BATON_CACHE_DIR` | Platform cache directory; holds import cursors and the session index (rebuilt when missing) |

- Files are only added, never changed, and session ids derive from the native session they came from, so machines can sync by copying files between data directories (for example through a git repository).
- A session continued on two machines keeps both branches; the newest is active and `baton show` lists the others.

## Worth knowing

- Agent sessions are only read during import. Exports create new sessions and never overwrite existing ones.
- Thinking is stored but never written back to an agent, tool calls are written as history and never run again, and auth, hooks, and MCP settings are untouched.
- Each agent's location variables are honored, such as `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `PASEO_HOME`, `XDG_DATA_HOME`, and `XDG_CONFIG_HOME`.
- OpenCode and Kilo Code sessions go through the `opencode` and `kilo` commands on `PATH`.
- Paseo keeps no conversations of its own: baton imports a Paseo agent through its provider's session (Claude Code, Codex, OpenCode, Pi, Oh My Pi), so the chat stays one baton session and remembers the Paseo agent with its title; archived and internal Paseo agents are skipped.
- `--to paseo` and `--in paseo` take `--provider` for `export` and `resume`; `materialize --in paseo` restores every session that was a Paseo agent somewhere under its original provider through `paseo` and a running Paseo daemon, and skips registration with a warning when the daemon is not running.
- DeepSeek Harness cannot open a session by id, so `baton resume --in dsh` starts `dsh web`, where the session is listed under its workspace or Ungrouped.

## Credits

The session format research builds on [session-migrate](https://github.com/xhluca/session-migrate) and [omnisession](https://github.com/bvolpato/omnisession).

## License

[MIT](LICENSE)
