use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};
use tempfile::TempDir;

pub(crate) struct Home {
    pub(crate) dir: TempDir,
}

impl Home {
    pub(crate) fn new() -> Home {
        let home = Home {
            dir: tempfile::tempdir().unwrap(),
        };
        fs::create_dir_all(home.project()).unwrap();
        home
    }

    pub(crate) fn path(&self, relative: &str) -> PathBuf {
        self.dir.path().join(relative)
    }

    pub(crate) fn project(&self) -> PathBuf {
        self.path("project")
    }

    pub(crate) fn host(&self) -> String {
        format!(
            "test-{}",
            self.dir.path().file_name().unwrap().to_string_lossy()
        )
    }

    pub(crate) fn baton(&self, args: &[&str]) -> String {
        self.baton_with_env(args, &[])
    }

    pub(crate) fn baton_with_env(
        &self,
        args: &[&str],
        variables: &[(&str, &std::ffi::OsStr)],
    ) -> String {
        let output = self.baton_output(args, variables);
        assert!(
            output.status.success(),
            "baton {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    pub(crate) fn baton_output(
        &self,
        args: &[&str],
        variables: &[(&str, &std::ffi::OsStr)],
    ) -> std::process::Output {
        let inherited = std::env::var_os("PATH").unwrap();
        let path = std::env::join_paths(
            std::iter::once(self.path("bin")).chain(std::env::split_paths(&inherited)),
        )
        .unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_baton"));
        command
            .args(args)
            .current_dir(self.project())
            .env_clear()
            .env("PATH", path)
            .env("HOME", self.dir.path())
            .env("USERPROFILE", self.dir.path())
            .env("TZ", "UTC")
            .env("BATON_HOME", self.path("baton"))
            .env("BATON_CACHE_DIR", self.path("cache"))
            .env("BATON_HOST", self.host());
        isolate_git(&mut command, &self.path("gitconfig"));
        command.envs(variables.iter().copied());
        command.output().unwrap()
    }

    pub(crate) fn run(&self, args: &[&str]) -> Output {
        self.baton_output(args, &[])
    }

    pub(crate) fn session(&self, id: &str) -> PathBuf {
        self.path(&format!("baton/sessions/{id}"))
    }

    pub(crate) fn sessions(&self, filter: &[&str]) -> Vec<String> {
        let mut args = vec!["list"];
        args.extend_from_slice(filter);
        self.baton(&args)
            .lines()
            .map(|line| line.split_whitespace().next().unwrap().to_owned())
            .collect()
    }

    pub(crate) fn event_count(&self, session: &str) -> usize {
        self.baton(&["list"])
            .lines()
            .find_map(|line| {
                let mut fields = line.split_whitespace();
                (fields.next()? == session).then(|| fields.nth(2).unwrap().parse().unwrap())
            })
            .unwrap()
    }

    pub(crate) fn pi_file(&self) -> PathBuf {
        self.path(".pi/agent/sessions/--work--/2026-01-01T00-00-00-000Z_pi-1.jsonl")
    }

    // Exports into the project directory, where baton runs, and returns the native id
    pub(crate) fn export(&self, session: &str, agent: &str) -> String {
        self.baton(&["export", session, "--to", agent, "--cwd", "."])
            .trim()
            .to_owned()
    }

    pub(crate) fn claude_transcript(&self, id: &str) -> PathBuf {
        let name = format!("{id}.jsonl");
        let matches: Vec<_> = self
            .claude_transcripts()
            .into_iter()
            .filter(|path| path.file_name().is_some_and(|file| file == name.as_str()))
            .collect();
        assert_eq!(matches.len(), 1, "expected one Claude transcript for {id}");
        matches.into_iter().next().unwrap()
    }

    pub(crate) fn continue_claude(
        &self,
        claude_id: &str,
        question: (&str, &str),
        answer: (&str, &str),
    ) {
        let transcript = self.claude_transcript(claude_id);
        let last = last_claude_uuid(&transcript);
        let project = self.project();
        let record = |uuid: &str, parent: &str, kind: &str, content: Value| {
            json!({"type": kind, "uuid": uuid, "parentUuid": parent, "sessionId": claude_id, "isSidechain": false,
                   "cwd": project, "timestamp": "2026-01-02T00:00:00.000Z", "message": {"role": kind, "content": content}})
        };
        append(
            &transcript,
            &[
                record(question.0, &last, "user", json!(question.1)),
                record(
                    answer.0,
                    question.0,
                    "assistant",
                    json!([{"type": "text", "text": answer.1}]),
                ),
            ],
        );
    }

    pub(crate) fn claude_transcripts(&self) -> Vec<PathBuf> {
        let projects = self.path(".claude/projects");
        if !projects.exists() {
            return Vec::new();
        }
        let mut paths: Vec<_> = fs::read_dir(projects)
            .unwrap()
            .flat_map(|entry| fs::read_dir(entry.unwrap().path()).unwrap())
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "jsonl")
            })
            .collect();
        paths.sort();
        paths
    }

    #[cfg(unix)]
    pub(crate) fn install_paseo(&self, running: bool) {
        use std::os::unix::fs::PermissionsExt;

        let script = if running {
            r##"#!/bin/sh
printf '%s\n' "$*" >> "$HOME/paseo.log"
case "$1" in
  ls) echo '[]' ;;
  import)
    if [ -f "$HOME/paseo-import-fails" ]; then exit 1; fi
    n=1
    if [ -f "$HOME/paseo.counter" ]; then n=$(cat "$HOME/paseo.counter"); fi
    echo $((n + 1)) > "$HOME/paseo.counter"
    id="restored-$n"
    mkdir -p "$HOME/.paseo/agents/work"
    printf '{"id":"%s","provider":"%s","cwd":"%s","title":null,"persistence":{"provider":"%s","sessionId":"%s","nativeHandle":"%s"}}' "$id" "$4" "$6" "$4" "$2" "$2" > "$HOME/.paseo/agents/work/$id.json"
    printf '{"agentId":"%s","status":"created","provider":"%s","cwd":"%s","title":null}\n' "$id" "$4" "$6"
    ;;
  delete)
    if [ -f "$HOME/paseo-delete-fails" ]; then exit 1; fi
    rm "$HOME/.paseo/agents/work/$2.json"
    ;;
esac
"##
        } else {
            "#!/bin/sh\nexit 1\n"
        };
        let executable = self.path("bin/paseo");
        fs::create_dir_all(executable.parent().unwrap()).unwrap();
        fs::write(&executable, script).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    pub(crate) fn registrations(&self) -> Vec<String> {
        let mut ids: Vec<String> = fs::read_dir(self.path(".paseo/agents/work"))
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                path.file_stem().unwrap().to_string_lossy().into_owned()
            })
            .collect();
        ids.sort();
        ids
    }

    #[cfg(unix)]
    pub(crate) fn export_to_paseo(&self, session: &str, provider: &str) -> String {
        self.baton(&["export", session, "--to", "paseo", "--provider", provider])
            .trim()
            .to_owned()
    }
}

pub(crate) fn jsonl(records: &[Value]) -> String {
    records.iter().map(|record| format!("{record}\n")).collect()
}

pub(crate) fn append(path: &Path, records: &[Value]) {
    use std::io::Write;

    let mut file = OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(jsonl(records).as_bytes()).unwrap();
}

pub(crate) fn isolate_git(command: &mut Command, global_config: &Path) {
    command
        .env("GIT_CONFIG_GLOBAL", global_config)
        .env("GIT_CONFIG_NOSYSTEM", "1");
    for variable in [
        "GIT_CONFIG",
        "GIT_CONFIG_SYSTEM",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_PARAMETERS",
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    ] {
        command.env_remove(variable);
    }
}

pub(crate) fn claude_session(home: &Home, id: &str) -> PathBuf {
    let file = home.path(&format!(".claude/projects/-work/{id}.jsonl"));
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(
        &file,
        jsonl(&[
            json!({"type": "user", "uuid": "a1", "parentUuid": null, "sessionId": id, "cwd": home.project(), "timestamp": "2026-01-01T00:00:00.000Z", "message": {"role": "user", "content": "Paseo question"}}),
        ]),
    )
    .unwrap();
    file
}

pub(crate) fn last_claude_uuid(transcript: &Path) -> String {
    fs::read_to_string(transcript)
        .unwrap()
        .lines()
        .filter_map(|line| {
            serde_json::from_str::<Value>(line).ok()?["uuid"]
                .as_str()
                .map(str::to_owned)
        })
        .next_back()
        .unwrap()
}
