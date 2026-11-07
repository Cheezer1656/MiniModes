//! A game's scripts: a tree of Luau modules keyed by path.

use std::collections::BTreeMap;

pub const ENTRY: &str = "main.luau";
pub const MAX_FILES: usize = 256;
pub const MAX_TOTAL_BYTES: usize = 2 << 20;

#[derive(Clone, Default, Debug)]
pub struct Bundle {
    /// Paths are `/`-separated and relative to the game's root, e.g. `lib/util.luau`.
    pub files: BTreeMap<String, String>,
}

impl Bundle {
    pub fn new(files: impl IntoIterator<Item = (String, String)>) -> Result<Self, String> {
        let files: BTreeMap<_, _> = files.into_iter().collect();
        if files.len() > MAX_FILES {
            return Err(format!("too many files ({}, the limit is {MAX_FILES})", files.len()));
        }
        let total: usize = files.iter().map(|(p, s)| p.len() + s.len()).sum();
        if total > MAX_TOTAL_BYTES {
            return Err(format!("scripts are too large ({total} bytes, the limit is {MAX_TOTAL_BYTES})"));
        }
        for path in files.keys() {
            if normalize(path).as_deref() != Some(path.as_str()) || !is_module(path) {
                return Err(format!("bad script path `{path}`"));
            }
        }
        Ok(Self { files })
    }

    pub fn has_entry(&self) -> bool {
        self.files.contains_key(ENTRY)
    }

    /// The module `require(name)` means when called from `from`. Names starting
    /// with `./` or `../` are relative to `from`'s directory, others to the root.
    /// `x` finds `x.luau`, `x.lua` or `x/init.luau`.
    pub fn resolve(&self, from: &str, name: &str) -> Option<&str> {
        let joined = if name.starts_with("./") || name.starts_with("../") {
            match from.rsplit_once('/') {
                Some((dir, _)) => format!("{dir}/{name}"),
                None => name.to_owned(),
            }
        } else {
            name.to_owned()
        };
        let base = normalize(&joined)?;
        [format!("{base}.luau"), format!("{base}.lua"), format!("{base}/init.luau"), base]
            .into_iter()
            .find_map(|p| self.files.get_key_value(&p).map(|(k, _)| k.as_str()))
            .filter(|p| is_module(p))
    }
}

fn is_module(path: &str) -> bool {
    path.ends_with(".luau") || path.ends_with(".lua")
}

/// Collapses `.` and `..`; `None` if the path escapes the root or is empty.
fn normalize(path: &str) -> Option<String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_relative_and_root_names() {
        let bundle = Bundle::new(
            ["main.luau", "lib/util.luau", "lib/shapes/init.luau", "config.lua"]
                .map(|p| (p.to_owned(), String::new())),
        )
        .unwrap();
        assert_eq!(bundle.resolve("main.luau", "lib/util"), Some("lib/util.luau"));
        assert_eq!(bundle.resolve("main.luau", "./config"), Some("config.lua"));
        assert_eq!(bundle.resolve("lib/util.luau", "./shapes"), Some("lib/shapes/init.luau"));
        assert_eq!(bundle.resolve("lib/util.luau", "../config"), Some("config.lua"));
        assert_eq!(bundle.resolve("lib/util.luau", "../../config"), None, "escapes the root");
        assert_eq!(bundle.resolve("main.luau", "missing"), None);
    }

    #[test]
    fn rejects_bad_paths() {
        for bad in ["../x.luau", "/abs.luau", "a//b.luau", "notes.txt", "a\\b.luau"] {
            assert!(Bundle::new([(bad.to_owned(), String::new())]).is_err(), "{bad}");
        }
    }
}
