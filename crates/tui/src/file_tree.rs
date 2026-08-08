//! Collapsible directory tree for the torrent files screen.
//!
//! Source of truth remains a flat [`FileProgress`] list; this module builds
//! visible rows with directory nodes for multi-file torrents.

use std::collections::{BTreeMap, HashSet};

use seedchamp_engine::FileProgress;

/// Aggregate wanted state for a directory of files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirWanted {
    All,
    None,
    Mixed,
}

/// One visible row on the files screen (directory or file).
#[derive(Debug, Clone)]
pub enum FileTreeRow {
    Dir {
        /// Last path component (display name).
        name: String,
        /// Full relative prefix, e.g. `foo/bar` (no trailing slash).
        prefix: String,
        depth: usize,
        expanded: bool,
        size: u64,
        have_bytes: u64,
        wanted: DirWanted,
        /// Indices into the flat `files` vec for all descendants.
        file_indices: Vec<usize>,
    },
    File {
        depth: usize,
        /// Index into the flat `files` vec.
        file_index: usize,
    },
}

/// Internal builder node.
struct Node {
    /// name → child dir
    dirs: BTreeMap<String, Node>,
    /// basename → file index (same dir can only have unique basenames in BT)
    files: BTreeMap<String, usize>,
}

impl Node {
    fn new() -> Self {
        Self {
            dirs: BTreeMap::new(),
            files: BTreeMap::new(),
        }
    }

    fn insert_path(&mut self, components: &[&str], file_index: usize) {
        match components {
            [] => {}
            [name] => {
                self.files.insert((*name).to_string(), file_index);
            }
            [name, rest @ ..] => {
                self.dirs
                    .entry((*name).to_string())
                    .or_insert_with(Node::new)
                    .insert_path(rest, file_index);
            }
        }
    }

    fn collect_file_indices(&self, out: &mut Vec<usize>) {
        for &idx in self.files.values() {
            out.push(idx);
        }
        for child in self.dirs.values() {
            child.collect_file_indices(out);
        }
    }
}

/// Split a torrent-relative path into components (`/` or `\`).
pub fn path_components(path: &str) -> Vec<&str> {
    path.split(['/', '\\'])
        .filter(|s| !s.is_empty() && *s != ".")
        .collect()
}

/// Build visible tree rows from flat file list.
///
/// `collapsed` holds directory **prefixes** that are collapsed (hidden children).
/// Empty set = all directories expanded.
pub fn build_visible_rows(files: &[FileProgress], collapsed: &HashSet<String>) -> Vec<FileTreeRow> {
    let mut root = Node::new();
    for (i, fp) in files.iter().enumerate() {
        let comps = path_components(&fp.file.path);
        if comps.is_empty() {
            // Degenerate path — still show as a file at root.
            root.files.insert(format!("file-{i}"), i);
        } else {
            root.insert_path(&comps, i);
        }
    }

    let mut out = Vec::new();
    flatten_node(&root, "", 0, files, collapsed, &mut out);
    out
}

fn flatten_node(
    node: &Node,
    prefix: &str,
    depth: usize,
    files: &[FileProgress],
    collapsed: &HashSet<String>,
    out: &mut Vec<FileTreeRow>,
) {
    // Directories first (BTreeMap sorted), then files.
    for (name, child) in &node.dirs {
        let child_prefix = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let mut indices = Vec::new();
        child.collect_file_indices(&mut indices);
        indices.sort_unstable();
        let (size, have_bytes, wanted) = aggregate(files, &indices);
        let expanded = !collapsed.contains(&child_prefix);
        out.push(FileTreeRow::Dir {
            name: name.clone(),
            prefix: child_prefix.clone(),
            depth,
            expanded,
            size,
            have_bytes,
            wanted,
            file_indices: indices,
        });
        if expanded {
            flatten_node(child, &child_prefix, depth + 1, files, collapsed, out);
        }
    }
    for &file_index in node.files.values() {
        out.push(FileTreeRow::File { depth, file_index });
    }
}

fn aggregate(files: &[FileProgress], indices: &[usize]) -> (u64, u64, DirWanted) {
    if indices.is_empty() {
        return (0, 0, DirWanted::None);
    }
    let mut size = 0u64;
    let mut have = 0u64;
    let mut any_on = false;
    let mut any_off = false;
    for &i in indices {
        let Some(fp) = files.get(i) else { continue };
        size = size.saturating_add(fp.file.size);
        have = have.saturating_add(fp.have_bytes.min(fp.file.size));
        if fp.wanted() {
            any_on = true;
        } else {
            any_off = true;
        }
    }
    let wanted = match (any_on, any_off) {
        (true, false) => DirWanted::All,
        (false, true) => DirWanted::None,
        (true, true) => DirWanted::Mixed,
        (false, false) => DirWanted::None,
    };
    (size, have, wanted)
}

/// Percent for aggregate dir/file (same rules as FileProgress::pct).
pub fn aggregate_pct(have_bytes: u64, size: u64) -> u32 {
    if size == 0 {
        return 100;
    }
    if have_bytes >= size {
        return 100;
    }
    ((100.0 * have_bytes as f64 / size as f64).floor() as u32).min(99)
}

#[cfg(test)]
mod tests {
    use super::*;
    use seedchamp_engine::FileRow;

    fn fp(path: &str, size: u64, prio: i32) -> FileProgress {
        FileProgress {
            file: FileRow {
                idx: 0,
                path: path.into(),
                size,
                offset: 0,
                priority: prio,
            },
            have_bytes: size / 2,
        }
    }

    #[test]
    fn builds_tree_and_collapses() {
        let files = vec![
            fp("a/x.txt", 100, 1),
            fp("a/y.txt", 100, 0),
            fp("b/z.txt", 50, 1),
        ];
        let collapsed = HashSet::new();
        let rows = build_visible_rows(&files, &collapsed);
        // a (dir), x, y, b (dir), z
        assert_eq!(rows.len(), 5);
        match &rows[0] {
            FileTreeRow::Dir {
                name,
                wanted,
                size,
                expanded,
                ..
            } => {
                assert_eq!(name, "a");
                assert_eq!(*wanted, DirWanted::Mixed);
                assert_eq!(*size, 200);
                assert!(*expanded);
            }
            _ => panic!("expected dir a"),
        }

        let mut collapsed = HashSet::new();
        collapsed.insert("a".into());
        let rows = build_visible_rows(&files, &collapsed);
        // a collapsed, b, z
        assert_eq!(rows.len(), 3);
        assert!(matches!(
            &rows[0],
            FileTreeRow::Dir {
                name,
                expanded: false,
                ..
            } if name == "a"
        ));
    }

    #[test]
    fn single_file_no_dir() {
        let files = vec![fp("solo.bin", 10, 1)];
        let rows = build_visible_rows(&files, &HashSet::new());
        assert_eq!(rows.len(), 1);
        assert!(matches!(
            rows[0],
            FileTreeRow::File {
                file_index: 0,
                depth: 0
            }
        ));
    }
}
