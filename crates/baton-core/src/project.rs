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
    Some(format!("{host}/{path}").to_lowercase())
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
    static REPOSITORIES: LazyLock<Mutex<HashMap<PathBuf, Repository>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    let mut repositories = REPOSITORIES.lock().unwrap();
    let Repository { remote, root } = repositories
        .entry(top.clone())
        .or_insert_with(|| {
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
        })
        .clone();

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

struct Checkouts {
    paths: Vec<(PathBuf, Project)>,
    remotes: HashMap<String, usize>,
    roots: HashMap<String, usize>,
}

fn checkouts() -> &'static Checkouts {
    static CHECKOUTS: LazyLock<Checkouts> = LazyLock::new(|| {
        let mut checkouts = Vec::new();
        if let Some(roots) = env::var_os("BATON_PROJECTS") {
            for root in env::split_paths(&roots) {
                scan(&root, 0, &mut checkouts);
            }
        }
        let mut remotes = HashMap::new();
        let mut roots = HashMap::new();
        for (index, (_, project)) in checkouts.iter().enumerate() {
            if let Some(remote) = &project.remote {
                remotes.entry(remote.clone()).or_insert(index);
            }
            if let Some(root) = &project.root {
                roots.entry(root.clone()).or_insert(index);
            }
        }
        Checkouts {
            paths: checkouts,
            remotes,
            roots,
        }
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
        let checkout = project
            .remote
            .as_ref()
            .and_then(|remote| checkouts().remotes.get(remote))
            .or_else(|| {
                project
                    .root
                    .as_ref()
                    .and_then(|root| checkouts().roots.get(root))
            });
        if let Some(index) = checkout {
            let checkout = &checkouts().paths[*index].0;
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
