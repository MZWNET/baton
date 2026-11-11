//! Project identity and local working directory resolution.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::{env, fs};

use serde::{Deserialize, Serialize};

// Each key is computed once and outside the map's lock, so threads compute different keys at once
type Cache<T> = LazyLock<Mutex<HashMap<PathBuf, Arc<OnceLock<T>>>>>;

fn cached<T: Clone>(cache: &Cache<T>, key: &Path, compute: impl FnOnce() -> T) -> T {
    let cell = cache
        .lock()
        .unwrap()
        .entry(key.to_path_buf())
        .or_default()
        .clone();

    cell.get_or_init(compute).clone()
}

/// The repository identity and the session directory relative to its root.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    /// The normalized URL of the repository's remote.
    pub remote: Option<String>,
    /// The lexicographically first root commit of the repository.
    pub root: Option<String>,
    /// The working directory relative to the repository root.
    pub path: String,
}

#[derive(Clone)]
struct Repository {
    remote: Option<String>,
    root: Option<String>,
}

fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn normalize_remote(remote: &str) -> Option<String> {
    let (host, path) = if let Some((_, rest)) = remote.split_once("://") {
        let (authority, path) = rest.split_once('/')?;
        let host = authority.rsplit('@').next()?.split(':').next()?;
        (host, path)
    } else {
        let (authority, path) = remote.split_once(':')?;
        (authority.rsplit('@').next()?, path)
    };
    let path = path
        .trim_matches('/')
        .strip_suffix(".git")
        .unwrap_or(path.trim_matches('/'));
    if host.is_empty() || path.is_empty() {
        return None;
    }
    let host = host.to_lowercase();
    let path = if host == "github.com" {
        path.to_lowercase()
    } else {
        path.to_owned()
    };
    Some(format!("{host}/{path}"))
}

fn inspect(cwd: &Path) -> Option<Project> {
    if !cwd.is_dir() {
        return None;
    }
    let canonical_cwd = fs::canonicalize(cwd).ok()?;
    if !canonical_cwd
        .ancestors()
        .any(|dir| dir.join(".git").exists())
    {
        return None;
    }
    let top = git(cwd, &["rev-parse", "--show-toplevel"])?;
    let top = fs::canonicalize(top).ok()?;
    let path = canonical_cwd
        .strip_prefix(&top)
        .ok()?
        .to_string_lossy()
        .into_owned();
    static REPOSITORIES: Cache<Repository> = LazyLock::new(Default::default);
    let Repository { remote, root } = cached(&REPOSITORIES, &top, || {
        let remotes = git(&top, &["remote"]).unwrap_or_default();
        let remote = remotes
            .lines()
            .find(|name| *name == "origin")
            .or_else(|| remotes.lines().min());
        let remote = remote
            .and_then(|name| git(&top, &["remote", "get-url", name]))
            .and_then(|url| normalize_remote(&url));
        let root = git(&top, &["rev-list", "--max-parents=0", "HEAD"])
            .and_then(|roots| roots.lines().min().map(str::to_owned));
        Repository { remote, root }
    });

    Some(Project { remote, root, path })
}

/// Identifies a local working directory, caching the result for this process.
pub fn identify(cwd: &Path) -> Option<Project> {
    static CACHE: Cache<Option<Project>> = LazyLock::new(Default::default);
    cached(&CACHE, cwd, || inspect(cwd))
}

fn same_repository(a: &Project, b: &Project) -> bool {
    match (&a.remote, &b.remote) {
        (Some(a), Some(b)) => a == b,
        _ => matches!((&a.root, &b.root), (Some(a), Some(b)) if a == b),
    }
}

fn scan(path: &Path, depth: usize, checkouts: &mut Vec<(PathBuf, Project)>) {
    if path.join(".git").exists() {
        if let Some(project) = identify(path) {
            checkouts.push((path.to_path_buf(), project));
        }
        return;
    }
    if depth == 3 {
        return;
    }
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    let mut children: Vec<_> = entries
        .filter_map(Result::ok)
        .filter(|entry| {
            !entry.file_name().to_string_lossy().starts_with('.')
                && entry.file_type().is_ok_and(|kind| kind.is_dir())
        })
        .map(|entry| entry.path())
        .collect();
    children.sort();
    for child in children {
        scan(&child, depth + 1, checkouts);
    }
}

pub(crate) fn checkouts() -> &'static [(PathBuf, Project)] {
    static CHECKOUTS: LazyLock<Vec<(PathBuf, Project)>> = LazyLock::new(|| {
        let mut checkouts = Vec::new();
        if let Some(roots) = env::var_os("BATON_PROJECTS") {
            for root in env::split_paths(&roots) {
                scan(&root, 0, &mut checkouts);
            }
        }

        checkouts
    });
    &CHECKOUTS
}

fn map_path(cwd: &Path) -> Option<PathBuf> {
    static MAPS: LazyLock<Vec<(PathBuf, PathBuf)>> = LazyLock::new(|| {
        let Some(maps) = env::var_os("BATON_PATH_MAP") else {
            return Vec::new();
        };
        env::split_paths(&maps)
            .filter_map(|entry| {
                let (from, to) = entry.to_str()?.split_once('=')?;
                Some((PathBuf::from(from), PathBuf::from(to)))
            })
            .collect()
    });
    let mut best = None;
    for (from, to) in MAPS.iter() {
        let Ok(suffix) = cwd.strip_prefix(from) else {
            continue;
        };
        let length = from.components().count();
        if best
            .as_ref()
            .is_none_or(|(previous, _, _)| length > *previous)
        {
            best = Some((length, to, suffix));
        }
    }
    best.map(|(_, to, suffix)| {
        if suffix.as_os_str().is_empty() {
            to.clone()
        } else {
            to.join(suffix)
        }
    })
    .filter(|path| path.exists())
}

/// Resolves a recorded project and directory to a directory on this machine.
pub fn resolve(project: Option<&Project>, cwd: Option<&str>) -> Option<PathBuf> {
    if let Some(project) = project {
        let remote_matches = checkouts().iter().filter(|(_, candidate)| {
            matches!((&project.remote, &candidate.remote), (Some(a), Some(b)) if a == b)
        });
        let root_matches = checkouts().iter().filter(|(_, candidate)| {
            (project.remote.is_none() || candidate.remote.is_none())
                && same_repository(project, candidate)
        });
        if let Some(path) = remote_matches
            .chain(root_matches)
            // Joining an empty path would add a trailing separator
            .map(|(checkout, _)| match project.path.as_str() {
                "" => checkout.clone(),
                path => checkout.join(path),
            })
            .find(|path| path.is_dir())
        {
            return Some(path);
        }
    }
    let cwd = Path::new(cwd?);
    map_path(cwd).or_else(|| cwd.is_dir().then(|| cwd.to_path_buf()))
}

pub(crate) fn matches(
    cwd: &Path,
    identity: Option<&Project>,
    recorded: Option<&str>,
    project: Option<&Project>,
) -> bool {
    if recorded.is_some_and(|recorded| Path::new(recorded) == cwd) {
        return true;
    }
    if let (Some(identity), Some(project)) = (identity, project)
        && identity.path == project.path
        && same_repository(identity, project)
    {
        return true;
    }
    recorded
        .and_then(|recorded| map_path(Path::new(recorded)))
        .is_some_and(|mapped| mapped == cwd)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    use super::{identify, normalize_remote, resolve};

    fn isolate_git(command: &mut Command, global_config: &Path) {
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

    fn git(cwd: &Path, args: &[&str]) {
        let mut command = Command::new("git");
        isolate_git(&mut command, &cwd.join("nonexistent-gitconfig"));
        let output = command.current_dir(cwd).args(args).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn project_resolution() {
        if let Some(root) = std::env::var_os("BATON_TEST_PROJECT_ROOT") {
            let root = Path::new(&root);
            let source = root.join("source");
            let checkouts = root.join("checkouts");
            let recorded = root.join("recorded");
            let mapped = root.join("mapped");
            let project = identify(&source).unwrap();
            assert_eq!(project.remote.as_deref(), Some("example.com/Team/Project"));

            let mut nested = project.clone();
            nested.path = "nested".into();
            assert_eq!(
                resolve(Some(&nested), None),
                Some(checkouts.join("c-match/nested"))
            );

            let mut other_case = project.clone();
            other_case.remote = Some("example.com/team/project".into());
            assert_eq!(
                resolve(Some(&other_case), None),
                Some(checkouts.join("a-other"))
            );

            let mut different = project.clone();
            different.remote = Some("example.com/TEAM/PROJECT".into());
            assert_eq!(
                resolve(Some(&different), recorded.to_str()),
                Some(recorded.clone())
            );

            let mut without_remote = project.clone();
            without_remote.remote = None;
            assert_eq!(
                resolve(Some(&without_remote), None),
                Some(checkouts.join("a-other"))
            );

            let mut missing = project;
            missing.path = "missing".into();
            let old_cwd = root.join("old/missing");
            assert_eq!(
                resolve(Some(&missing), old_cwd.to_str()),
                Some(mapped.join("missing"))
            );

            return;
        }

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let source = root.join("source");
        let checkouts = root.join("checkouts");
        let recorded = root.join("recorded");
        let mapped = root.join("mapped");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&checkouts).unwrap();
        fs::create_dir_all(&recorded).unwrap();
        fs::create_dir_all(mapped.join("missing")).unwrap();
        git(&source, &["init"]);
        git(
            &source,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--allow-empty",
                "-m",
                "initial",
            ],
        );
        for (name, remote) in [
            ("a-other", "https://EXAMPLE.com/team/project.git"),
            ("b-match", "git@EXAMPLE.com:Team/Project.git"),
            ("c-match", "ssh://git@example.com:22/Team/Project.git"),
        ] {
            git(&checkouts, &["clone", source.to_str().unwrap(), name]);
            git(
                &checkouts.join(name),
                &["remote", "set-url", "origin", remote],
            );
        }
        fs::create_dir_all(checkouts.join("c-match/nested")).unwrap();
        git(
            &source,
            &[
                "remote",
                "add",
                "origin",
                "https://EXAMPLE.com/Team/Project.git",
            ],
        );
        let path_map = format!("{}={}", root.join("old").display(), mapped.display());
        let mut command = Command::new(std::env::current_exe().unwrap());
        isolate_git(&mut command, &root.join("nonexistent-gitconfig"));
        let output = command
            .args(["--exact", "project::tests::project_resolution"])
            .env("BATON_TEST_PROJECT_ROOT", root)
            .env("BATON_PROJECTS", &checkouts)
            .env("BATON_PATH_MAP", path_map)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("test result: ok. 1 passed"));
    }

    #[test]
    fn remote_normalization() {
        for remote in [
            "git@github.com:MZWNET/baton.git",
            "https://GITHUB.COM/MZWNET/baton",
            "ssh://git@github.com:22/MZWNET/baton.git/",
        ] {
            assert_eq!(
                normalize_remote(remote).as_deref(),
                Some("github.com/mzwnet/baton")
            );
        }
    }
}
