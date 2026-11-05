use std::process::Command;

// On Windows, `Command` only looks for `<name>.exe` on PATH, while npm installs agents as `.cmd` shims
pub(crate) fn command(name: &str) -> Command {
    #[cfg(windows)]
    if let Some(path) = find_on_path(name) {
        return Command::new(path);
    }
    Command::new(name)
}

#[cfg(windows)]
fn find_on_path(name: &str) -> Option<std::path::PathBuf> {
    use std::env;

    let extensions = env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
    let extensions: Vec<&str> = extensions
        .split(';')
        .filter(|extension| !extension.is_empty())
        .collect();
    for dir in env::split_paths(&env::var_os("PATH")?) {
        for extension in &extensions {
            let path = dir.join(format!("{name}{extension}"));
            if path.is_file() {
                return Some(path);
            }
        }
    }
    None
}
