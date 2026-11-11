//! Export validation, native batch persistence, and store lock regressions

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use std::{env, fs, thread};

use anyhow::{Result, bail};
use baton_core::{
    Adapter, Agent, Baton, Blobs, Body, Cursor, Export, ImportOutcome, Info, NativeRead,
    NativeSession, Progress, Source, Stamp, User,
};
use uuid::Uuid;

const ROOT: &str = "BATON_EXPORT_COMMIT_TEST_ROOT";
const READY: &str = "BATON_EXPORT_COMMIT_LOCK_READY";
const STARTED: &str = "2026-01-01T00:00:00.000Z";

type Transcripts = HashMap<String, Vec<Body>>;
type FlushHook<'a> = &'a (dyn Fn() -> Result<()> + Sync);

fn no_hook() -> Result<()> {
    Ok(())
}

// Real Store and cursors, with a native adapter whose batch stays private until flushed
struct DeferredAdapter<'a> {
    agent: Agent,
    path: PathBuf,
    staged: Mutex<Transcripts>,
    before_flush: FlushHook<'a>,
}

impl DeferredAdapter<'_> {
    fn open(agent: Agent, root: &Path) -> Self {
        let path = root.join(format!("{agent}.json"));
        Self {
            agent,
            staged: Mutex::new(Self::committed(&path)),
            path,
            before_flush: &no_hook,
        }
    }

    fn committed(path: &Path) -> Transcripts {
        match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(error) => panic!("reading {}: {error}", path.display()),
        }
    }

    fn native(&self, id: &str, len: usize) -> NativeSession {
        NativeSession {
            id: id.into(),
            path: self.path.clone(),
            stamp: Stamp {
                size: len as u64,
                mtime: 0,
            },
            title: None,
        }
    }

    fn set(&mut self, id: &str, bodies: Vec<Body>) -> NativeSession {
        let native = self.native(id, bodies.len());
        self.staged.get_mut().unwrap().insert(id.into(), bodies);

        native
    }
}

impl Source for DeferredAdapter<'_> {
    fn agent(&self) -> Agent {
        self.agent
    }

    fn discover(&self, _path: Option<&Path>) -> Result<Vec<NativeSession>> {
        Ok(self
            .staged
            .lock()
            .unwrap()
            .iter()
            .map(|(id, bodies)| self.native(id, bodies.len()))
            .collect())
    }

    fn read(
        &self,
        native: &NativeSession,
        _cursor: Option<&Cursor>,
        _blobs: &Blobs,
    ) -> Result<NativeRead> {
        let staged = self.staged.lock().unwrap();
        let drafts = staged[&native.id]
            .iter()
            .cloned()
            .map(|body| baton_core::Draft {
                timestamp: STARTED.into(),
                native_entry_id: None,
                body,
            })
            .collect();
        Ok(NativeRead {
            info: Info {
                cwd: self
                    .path
                    .parent()
                    .map(|path| path.to_string_lossy().into_owned()),
                created: Some(STARTED.into()),
                ..Info::default()
            },
            drafts,
            continued: false,
            progress: Progress::default(),
        })
    }
}

impl Adapter for DeferredAdapter<'_> {
    fn write(&self, export: &Export, _blobs: &Blobs) -> Result<NativeSession> {
        let id = export.seed.unwrap_or_else(Uuid::new_v4).to_string();
        let mut staged = self.staged.lock().unwrap();
        if staged.contains_key(&id) {
            bail!("native session {id} already exists");
        }
        let bodies: Vec<_> = export
            .events
            .iter()
            .map(|event| event.body.clone())
            .collect();
        let native = self.native(&id, bodies.len());
        staged.insert(id, bodies);

        Ok(native)
    }

    fn remove(&self, session: &NativeSession) -> Result<()> {
        self.staged.lock().unwrap().remove(&session.id);
        Ok(())
    }

    fn flush(&self) -> Result<()> {
        let staged = self.staged.lock().unwrap();
        (self.before_flush)()?;
        fs::write(&self.path, serde_json::to_vec(&*staged)?)?;

        Ok(())
    }

    fn resume_command(&self, _session: &NativeSession, _cwd: &Path) -> Command {
        unreachable!("these tests do not resume native sessions")
    }
}

fn user(text: &str) -> Body {
    Body::User(User {
        text: text.into(),
        images: Vec::new(),
    })
}

fn child(test: &str, root: &Path) -> Command {
    let mut command = Command::new(env::current_exe().unwrap());
    command
        .args(["--exact", test])
        .env(ROOT, root)
        .env("BATON_HOME", root.join("data"))
        .env("BATON_CACHE_DIR", root.join("cache"))
        .env("BATON_HOST", "export-test")
        .env_remove("BATON_PROJECTS")
        .env_remove("BATON_PATH_MAP")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    command
}

fn assert_child(output: Output) {
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn isolated(test: &str, run: impl FnOnce(Baton, &Path)) {
    if let Some(root) = env::var_os(ROOT) {
        run(Baton::open().unwrap(), Path::new(&root));
    } else {
        let root = tempfile::tempdir().unwrap();
        assert_child(child(test, root.path()).output().unwrap());
    }
}

fn import_new(baton: &mut Baton, source: &DeferredAdapter, native: &NativeSession) -> String {
    let ImportOutcome::Created { session } = baton.import(source, native).unwrap() else {
        panic!("expected a new session");
    };
    baton.refresh().unwrap();

    session
}

#[test]
fn materialization_publishes_after_successful_flush() {
    isolated(
        "materialization_publishes_after_successful_flush",
        |mut baton, root| {
            let mut source = DeferredAdapter::open(Agent::Pi, root);
            let original = source.set("first", vec![user("old")]);
            let first = import_new(&mut baton, &source, &original);
            let adapter = DeferredAdapter::open(Agent::Claude, root);
            let old = baton.export(&first, &adapter, Some(root)).unwrap().native;
            drop(adapter);

            let updated = source.set("first", vec![user("old"), user("new")]);
            assert!(matches!(
                baton.import(&source, &updated).unwrap(),
                ImportOutcome::Appended { events: 1, .. }
            ));
            let another = source.set("second", vec![user("another")]);
            let second = import_new(&mut baton, &source, &another);
            let fail = || bail!("native persistence failed");
            let mut failed = DeferredAdapter::open(Agent::Claude, root);
            failed.before_flush = &fail;
            let natives = failed.discover(None).unwrap();
            assert!(
                baton
                    .materialize(&failed, &natives, None, &HashSet::new())
                    .is_err()
            );
            drop(baton);
            let baton = Baton::open().unwrap();
            let first_links = baton.store.load(&first).unwrap().meta.links;
            assert_eq!(
                first_links
                    .iter()
                    .filter(|link| link.agent == Agent::Claude)
                    .map(|link| &link.native_id)
                    .collect::<HashSet<_>>(),
                HashSet::from([&old.id])
            );
            assert!(
                !baton
                    .store
                    .load(&second)
                    .unwrap()
                    .meta
                    .links
                    .iter()
                    .any(|link| link.agent == Agent::Claude)
            );
            let backup = DeferredAdapter::committed(&failed.path);
            assert_eq!(
                backup.keys().collect::<HashSet<_>>(),
                HashSet::from([&old.id])
            );

            let adapter = DeferredAdapter::open(Agent::Claude, root);
            let natives = adapter.discover(None).unwrap();
            baton
                .materialize(&adapter, &natives, None, &HashSet::new())
                .unwrap();
            drop(baton);
            let baton = Baton::open().unwrap();
            assert_eq!(
                baton.index.linked_session(Agent::Claude, &old.id).unwrap(),
                None
            );
            let backup = DeferredAdapter::committed(&adapter.path);
            let mut exported = HashSet::new();
            for session in [&first, &second] {
                let native = baton
                    .index
                    .latest_native(session, Agent::Claude)
                    .unwrap()
                    .unwrap();
                let links = baton.store.load(session).unwrap().meta.links;
                assert_eq!(
                    links
                        .iter()
                        .filter(|link| link.agent == Agent::Claude)
                        .map(|link| &link.native_id)
                        .collect::<HashSet<_>>(),
                    HashSet::from([&native])
                );
                exported.insert(native);
            }
            assert_eq!(backup.keys().cloned().collect::<HashSet<_>>(), exported);

            let reopened = DeferredAdapter::open(Agent::Claude, root);
            let natives = reopened.discover(None).unwrap();
            baton
                .materialize(&reopened, &natives, None, &HashSet::new())
                .unwrap();
            drop(baton);
            let baton = Baton::open().unwrap();
            for session in [&first, &second] {
                let native = baton
                    .index
                    .latest_native(session, Agent::Claude)
                    .unwrap()
                    .unwrap();
                assert!(exported.contains(&native));
            }
            assert_eq!(DeferredAdapter::committed(&reopened.path), backup);
        },
    );
}

#[test]
fn the_same_data_directory_serializes_distinct_caches() {
    let test = "the_same_data_directory_serializes_distinct_caches";
    if let Some(ready) = env::var_os(READY) {
        fs::write(ready, "").unwrap();
        let _baton = Baton::open().unwrap();
        return;
    }
    isolated(test, |baton, root| {
        #[cfg(unix)]
        let data = {
            let alias = root.join("alias");
            std::os::unix::fs::symlink(root.join("data"), &alias).unwrap();
            alias
        };
        #[cfg(not(unix))]
        let data = root.join("data").join(".");
        let ready = root.join("contender-ready");
        let mut contender = child(test, root)
            .env("BATON_HOME", data)
            .env("BATON_CACHE_DIR", root.join("other-cache"))
            .env(READY, &ready)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready.is_file()
            && Instant::now() < deadline
            && contender.try_wait().unwrap().is_none()
        {
            thread::sleep(Duration::from_millis(10));
        }
        let started = ready.is_file();
        thread::sleep(Duration::from_millis(250));
        let blocked = contender.try_wait().unwrap().is_none();
        drop(baton);
        assert_child(contender.wait_with_output().unwrap());
        assert!(started, "contender did not reach Baton::open");
        assert!(
            blocked,
            "contender opened the same data directory before the holder released it"
        );
    });
}
