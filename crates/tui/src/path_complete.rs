//! Path completion for TUI command palette / relocate.

use std::path::{Path, PathBuf};

pub(crate) fn display_prefix_for_complete(input: &str) -> String {
    let expanded = seedchamp_engine::expand_user_path(input.trim());
    let mut s = expanded.display().to_string();
    if input.ends_with('/') && !s.ends_with('/') {
        s.push('/');
    }
    s
}

/// List absolute path completions for `input` (dirs end with `/`).
pub(crate) fn list_path_completions(input: &str) -> Vec<String> {
    let raw = input.trim();
    if raw.is_empty() {
        return list_dir_entries(Path::new("/"), "", true);
    }

    let home_prefix = raw.starts_with("~/") || raw == "~";
    let expanded = seedchamp_engine::expand_user_path(raw);
    let ends_slash = raw.ends_with('/');

    let (dir, prefix) = if ends_slash {
        (expanded.as_path(), String::new())
    } else {
        let parent = expanded
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("/"));
        let name = expanded
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        // "/foo" with no slash after root: parent is "/", name is "foo" — OK.
        // "foo" relative: parent may be "" — treat as cwd.
        let dir = if expanded.is_relative()
            && expanded
                .parent()
                .map(|p| p.as_os_str().is_empty())
                .unwrap_or(true)
        {
            Path::new(".")
        } else {
            parent
        };
        (dir, name)
    };

    let mut matches = list_dir_entries(dir, &prefix, home_prefix);
    matches.sort();
    matches.dedup();
    matches
}

pub(crate) fn list_dir_entries(dir: &Path, prefix: &str, prefer_tilde: bool) -> Vec<String> {
    let rd = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for ent in rd.flatten() {
        let name = ent.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') && !prefix.starts_with('.') {
            continue; // hide dotfiles unless typing one
        }
        if !name.starts_with(prefix) {
            continue;
        }
        let is_dir = ent.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let full = dir.join(name.as_ref());
        if is_dir {
            // Keep trailing slash so next Tab lists children.
            let mut s = path_to_display(&full, prefer_tilde);
            if !s.ends_with('/') {
                s.push('/');
            }
            out.push(s);
        } else {
            out.push(path_to_display(&full, prefer_tilde));
        }
    }
    out
}

pub(crate) fn path_to_display(p: &Path, prefer_tilde: bool) -> String {
    if prefer_tilde {
        if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
            let home = PathBuf::from(home);
            if let Ok(rel) = p.strip_prefix(&home) {
                if rel.as_os_str().is_empty() {
                    return "~/".into();
                }
                return format!("~/{}", rel.display());
            }
        }
    }
    p.display().to_string()
}

pub(crate) fn longest_common_prefix(paths: &[String]) -> String {
    if paths.is_empty() {
        return String::new();
    }
    let mut prefix = paths[0].clone();
    for s in &paths[1..] {
        while !s.starts_with(&prefix) {
            if prefix.is_empty() {
                break;
            }
            prefix.pop();
        }
    }
    // Don't leave a partial multi-byte char (ASCII paths only in practice).
    prefix
}
