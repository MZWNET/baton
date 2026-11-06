use crate::support::files;

// Claude Code names a project directory after its dashed path, and shortens long names with a hash
pub(super) fn from_cwd(cwd: &str) -> String {
    let mut key = files::dashed(cwd);
    if key.len() > 200 {
        let hash = cwd.encode_utf16().fold(0i32, |hash, unit| {
            hash.wrapping_mul(31).wrapping_add(i32::from(unit))
        });
        key.truncate(200);
        key.push('-');
        key.push_str(&base36(i64::from(hash).unsigned_abs()));
    }
    key
}

fn base36(mut value: u64) -> String {
    let mut digits = Vec::new();
    loop {
        digits.push(char::from_digit((value % 36) as u32, 36).expect("remainders stay below 36"));
        value /= 36;
        if value == 0 {
            break;
        }
    }
    digits.iter().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_keys_follow_claude_naming() {
        assert_eq!(from_cwd("/work/a.b"), "-work-a-b");
        assert_eq!(from_cwd("/tmp/项目"), "-tmp---");
        let key = from_cwd(&format!("/{}", "x".repeat(250)));
        assert!(
            key.starts_with(&format!("-{}-", "x".repeat(199))) && key.len() >= 202,
            "long paths keep 200 characters plus a hash suffix: {key}"
        );
    }
}
