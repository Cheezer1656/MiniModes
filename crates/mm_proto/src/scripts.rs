//! Rules for a game's script files, shared by the editor (lobby) and the
//! script runtime (shards) so both reject the same things.

/// The module a game starts from.
pub const ENTRY: &str = "main.luau";
pub const MAX_FILES: usize = 256;
/// Paths plus sources, summed over every file.
pub const MAX_TOTAL_BYTES: usize = 2 << 20;
pub const MAX_PATH_LEN: usize = 200;

pub fn is_module(path: &str) -> bool {
    path.ends_with(".luau") || path.ends_with(".lua")
}

/// Collapses `.` and `..`; `None` if the path escapes the root or is empty.
pub fn normalize(path: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            p if p.contains('\\') => return None,
            p => parts.push(p),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// A path as stored: already normalized, a `.luau`/`.lua` module, and made
/// of plain characters (letters, digits, `_ - .` and `/` between parts).
pub fn check_path(path: &str) -> Result<(), String> {
    if path.len() > MAX_PATH_LEN {
        return Err(format!("paths can be at most {MAX_PATH_LEN} characters"));
    }
    if normalize(path).as_deref() != Some(path) {
        return Err(format!("`{path}` isn't a clean relative path"));
    }
    if !path
        .split('/')
        .all(|part| !part.starts_with('.') && part.chars().all(|c| c.is_ascii_alphanumeric() || "_-.".contains(c)))
    {
        return Err(format!("`{path}` may only use letters, digits, `_`, `-` and `.`"));
    }
    if !is_module(path) {
        return Err(format!("`{path}` must end in `.luau` or `.lua`"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_plain_module_paths() {
        for ok in ["main.luau", "lib/util.luau", "a-b/c_d.lua", "x/init.luau"] {
            assert_eq!(check_path(ok), Ok(()), "{ok}");
        }
    }

    #[test]
    fn rejects_bad_paths() {
        for bad in [
            "../x.luau",
            "/abs.luau",
            "a//b.luau",
            "a/./b.luau",
            "notes.txt",
            "a\\b.luau",
            ".hidden.luau",
            "sp ace.luau",
            "ünï.luau",
            "",
        ] {
            assert!(check_path(bad).is_err(), "{bad}");
        }
        assert!(check_path(&format!("{}.luau", "a".repeat(MAX_PATH_LEN))).is_err());
    }
}
