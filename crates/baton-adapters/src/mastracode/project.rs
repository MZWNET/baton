use std::path::Path;

use crate::support::{files, identifiers, program};

// The resource MastraCode files a directory's threads under, and the project path it records for them
pub(super) struct Project {
    pub(super) resource: String,
    pub(super) path: String,
}

// MastraCode only lists and resumes threads of the resource it derives from the git remote, the main repository or the directory
pub(super) fn detect(cwd: &Path) -> Project {
    let git = |args: &[&str]| {
        let output = program::command("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        (output.status.success() && !text.is_empty()).then_some(text)
    };
    let mut path = cwd.to_string_lossy().into_owned();
    let mut main_repository = None;
    let mut url = None;
    if let Some(git_dir) = git(&["rev-parse", "--git-dir"]) {
        path = git(&["rev-parse", "--show-toplevel"]).unwrap_or(path);
        // A worktree shares its main repository's common directory
        if let Some(common) = git(&["rev-parse", "--git-common-dir"])
            .filter(|common| common != ".git" && *common != git_dir)
        {
            let common = Path::new(&path).join(common);
            main_repository = common
                .parent()
                .map(|main| main.to_string_lossy().into_owned());
        }
        url = git(&["remote", "get-url", "origin"]).or_else(|| {
            let remotes = git(&["remote"])?;
            git(&["remote", "get-url", remotes.lines().next()?])
        });
    }
    let (source, base) = match &url {
        Some(url) => {
            let name = url.rsplit('/').next().unwrap_or(url);
            let name = name.strip_suffix(".git").unwrap_or(name);
            let base = if name.is_empty() { "project" } else { name };
            (normalized(url), base.to_owned())
        }
        None => {
            let base = Path::new(&path)
                .file_name()
                .map_or(String::new(), |name| name.to_string_lossy().into_owned());
            (main_repository.unwrap_or_else(|| path.clone()), base)
        }
    };
    let resource = match files::env_dir("MASTRA_RESOURCE_ID") {
        Some(resource) => resource.to_string_lossy().into_owned(),
        None => format!(
            "{}-{}",
            slug(&base),
            &identifiers::sha256_hex(source.as_bytes())[..12]
        ),
    };
    Project { resource, path }
}

// SSH and HTTPS remotes of one repository normalize to the same URL
fn normalized(url: &str) -> String {
    let url = url.strip_suffix(".git").unwrap_or(url);
    let url = match url
        .strip_prefix("git@")
        .and_then(|rest| rest.split_once(':'))
    {
        Some((host, path)) => format!("https://{host}/{path}"),
        None => url.to_owned(),
    };
    let url = match url.strip_prefix("ssh://git@") {
        Some(rest) => format!("https://{rest}"),
        None => url,
    };
    url.to_lowercase()
}

fn slug(base: &str) -> String {
    let mut slug = String::with_capacity(base.len());
    for char in base.to_lowercase().chars() {
        if char.is_ascii_lowercase() || char.is_ascii_digit() {
            slug.push(char);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.strip_prefix('-').unwrap_or(&slug);
    slug.strip_suffix('-').unwrap_or(slug).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remotes_and_names_normalize_like_mastracode() {
        assert_eq!(
            normalized("git@GitHub.com:Org/Repo.git"),
            "https://github.com/org/repo"
        );
        assert_eq!(normalized("ssh://git@host/x/y.git"), "https://host/x/y");
        assert_eq!(slug("--My Project_2--"), "my-project-2");
        assert_eq!(slug("项目"), "");
    }
}
