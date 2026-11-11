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
| [RikkaHub](https://github.com/rikkahub/rikkahub) | `rikkahub` | Read / Write |

## Install

- Tagged versions are on [Releases](https://github.com/MZWNET/baton/releases).
- Preview builds are in the artifacts of the newest [Build](https://github.com/MZWNET/baton/actions/workflows/build.yml?query=branch%3Amaster) workflow run.
- Nix: `nix profile add github:MZWNET/baton`, or use the flake's `overlays.default` (`pkgs.baton`).
- From source: `cargo install --path crates/baton-cli`.

## Usage

```sh
baton import [--agent omp] [--path <path>]      # read new and changed sessions
baton import --agent rikkahub --path <file>     # read the conversations of a RikkaHub backup
baton list [--cwd [<dir>]] [--agent omp]
baton show <id>
baton export <id> --to claude [--cwd <dir>]     # write a new Claude Code session and print its id
baton export <id> --to paseo --provider claude  # write it for Claude Code, then register it in Paseo
baton export <id> --to rikkahub --backup <file> # add the session to a RikkaHub backup
baton resume <id> --in codex [--cwd <dir>]      # export, then open it in Codex
baton materialize --in omp --in claude          # write every session into each agent, in the matching local project
baton migrate                                   # upgrade a data directory written by an older baton to the current format
```

`<id>` is a baton session id, a unique prefix of it, or `agent:native_id` such as `omp:01a1a978`. For `rikkahub:<id>`, import the backup first.

### Syncing and continuing

- Imports are incremental and idempotent. When `--path` belongs to one agent, pass `--agent` so only that agent reads it.
- A session exported to another agent and continued there flows back into the same baton session on the next import, as a fork if the baton session moved on meanwhile.
- `baton materialize --in <agent>` (repeatable) imports that agent, then writes every session it lacks on this machine so its resume list shows them; rerunning writes nothing new.
- The data directory only ever gains files and session ids derive from the native session, so machines sync by copying files between data directories (for example through a git repository); a session continued on two machines keeps both branches, the newest is active and `baton show` lists the others.
- `export`, `resume`, and `materialize` place a session in the checkout under `BATON_PROJECTS` with the same git identity (the git remote, `origin` preferred, or the root commit when either side has no remote, where the directory inside the repository already exists), falling back to `BATON_PATH_MAP` and then the recorded directory; `list --cwd` also matches sessions recorded under another machine's path of the same repository.

## Configuration

| Variable | Purpose |
| --- | --- |
| `BATON_HOME` | Data directory; defaults to `~/.local/share/baton` (Linux), `~/Library/Application Support/baton` (macOS), `%APPDATA%\baton\data` (Windows) |
| `BATON_HOST` | Identifies this machine in the synced data; defaults to the host name |
| `BATON_CACHE_DIR` | Import cursors and the session index (rebuilt when missing); defaults to the platform cache directory |
| `BATON_PROJECTS` | `PATH`-separated directories searched up to three levels deep for matching git checkouts |
| `BATON_PATH_MAP` | `PATH`-separated `from=to` path prefix pairs used when no checkout matches (for example `/root/repos=/Users/me/repos`) |
| `RAYON_NUM_THREADS` | Limits the threads used by imports, `materialize`, and session index rebuilds, which use all CPU cores by default |
| `TZ` | Time zone for RikkaHub and OpenHands timestamps that lack a UTC offset; defaults to the system zone |

Each agent's own location variables are honored, such as `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `PASEO_HOME`, `XDG_DATA_HOME`, and `XDG_CONFIG_HOME`.

## Worth knowing

- Agent sessions are only read during import; exports create new sessions and never overwrite existing ones.
- Before running `baton materialize --in codex`, close older Codex versions that do not use the shared `session_index.jsonl.lock` for title updates.
- Thinking is stored but never written back, tool calls are written as history and never run again, and auth, hooks, and MCP settings are untouched.
- OpenCode and Kilo Code sessions go through the `opencode` and `kilo` commands on `PATH`, and the project directory must exist to receive a session.
- Paseo keeps no conversations of its own: baton imports a Paseo agent through its provider's session (Claude Code, Codex, OpenCode, Pi, Oh My Pi), so the chat stays one baton session.
- `export --to paseo` and `resume --in paseo` take `--provider`, while `materialize --in paseo` restores every session that was a Paseo agent somewhere under each provider it was registered with; registering goes through `paseo` and a running Paseo daemon, and when the daemon is not running `materialize --in paseo` writes only the provider sessions and warns that registration and deletion are skipped.
- DeepSeek Harness cannot open a session by id, so `baton resume --in dsh` starts `dsh web`, where the session is listed under its workspace or Ungrouped.
- RikkaHub runs on Android, so baton reads backups exported from the app; `--backup <file>` with `export` or `materialize` adds sessions to that backup and creates it when missing, and `resume` cannot open RikkaHub. Restoring a backup in the app replaces all of its conversations, so add sessions to a backup made right before restoring.

## Credits

The session format research builds on [session-migrate](https://github.com/xhluca/session-migrate) and [omnisession](https://github.com/bvolpato/omnisession).

## License

[MIT](LICENSE)
