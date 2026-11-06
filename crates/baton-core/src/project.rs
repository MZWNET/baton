//! Project identity and local working directory resolution.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{LazyLock, Mutex};
use std::{env, fs};

use serde::{Deserialize, Serialize};

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
    Some(format!("{host}/{path}").to_lowercase())
}

fn inspect(cwd: &Path) -> Option<Project> {
    if !cwd.is_dir() {
        return None;
    }
    let top = git(cwd, &["rev-parse", "--show-toplevel"])?;
    let cwd = fs::canonicalize(cwd).ok()?;
    let top = fs::canonicalize(top).ok()?;
    let path = cwd.strip_prefix(&top).ok()?.to_string_lossy().into_owned();
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
    Some(Project { remote, root, path })
}

/// Identifies a local working directory, caching the result for this process.
pub fn identify(cwd: &Path) -> Option<Project> {
    static CACHE: LazyLock<Mutex<HashMap<PathBuf, Option<Project>>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    let mut cache = CACHE.lock().unwrap();
    cache
        .entry(cwd.to_path_buf())
        .or_insert_with(|| inspect(cwd))
        .clone()
}

fn same_repository(a: &Project, b: &Project) -> bool {
    matches!((&a.remote, &b.remote), (Some(a), Some(b)) if a == b)
        || matches!((&a.root, &b.root), (Some(a), Some(b)) if a == b)
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

fn checkouts() -> &'static [(PathBuf, Project)] {
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
    let maps = env::var_os("BATON_PATH_MAP")?;
    let mut best = None;
    for entry in env::split_paths(&maps) {
        let Some(entry) = entry.to_str() else {
            continue;
        };
        let Some((from, to)) = entry.split_once('=') else {
            continue;
        };
        let from = Path::new(from);
        let Ok(suffix) = cwd.strip_prefix(from) else {
            continue;
        };
        let length = from.components().count();
        if best.as_ref().is_none_or(|(previous, _)| length > *previous) {
            let to = PathBuf::from(to);
            let path = if suffix.as_os_str().is_empty() {
                to
            } else {
                to.join(suffix)
            };
            best = Some((length, path));
        }
    }
    best.map(|(_, path)| path).filter(|path| path.exists())
}

/// Resolves a recorded project and directory to a directory on this machine.
pub fn resolve(project: Option<&Project>, cwd: Option<&str>) -> Option<PathBuf> {
    if let Some(project) = project {
        let remote = project.remote.as_ref().and_then(|remote| {
            checkouts()
                .iter()
                .find(|(_, candidate)| candidate.remote.as_ref() == Some(remote))
        });
        let checkout = remote.or_else(|| {
            project.root.as_ref().and_then(|root| {
                checkouts()
                    .iter()
                    .find(|(_, candidate)| candidate.root.as_ref() == Some(root))
            })
        });
        if let Some((checkout, _)) = checkout {
            return Some(if project.path.is_empty() {
                checkout.clone()
            } else {
                checkout.join(&project.path)
            });
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
    use super::normalize_remote;

    #[test]
    fn remote_normalization() {
        for remote in [
            "git@github.com:MZWNET/baton.git",
            "https://github.com/MZWNET/baton",
            "ssh://git@github.com:22/MZWNET/baton.git",
        ] {
            assert_eq!(
                normalize_remote(remote).as_deref(),
                Some("github.com/mzwnet/baton")
            );
        }
    }
}
