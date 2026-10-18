//! The worktree's file tree, built once from the VCS file list.
//!
//! Nodes sit in one array in display order (depth first, folders before
//! files, names without case), so a folder's subtree is the range up to its
//! `end`. Names are slices of the listing the VCS printed. Showing the tree
//! walks that array and skips the subtrees of collapsed folders; nothing per
//! row is allocated.

use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug)]
struct Node {
    /// Byte range of the name in `Tree::names`.
    name: (u32, u32),
    parent: u32,
    /// Index after the last node of this node's subtree.
    end: u32,
    depth: u16,
    folder: bool,
}

/// Index of a node in display order.
pub type NodeId = u32;

const NO_PARENT: u32 = u32::MAX;

pub struct Tree {
    names: Vec<u8>,
    nodes: Vec<Node>,
    expanded: Vec<bool>,
    /// Status letter of each node from the VCS, zero for an unchanged one.
    /// A folder carries its changed files' letter, or `M` when they differ.
    changes: Vec<u8>,
    files: usize,
}

/// A node while the tree is built, before it has its place.
struct Draft {
    name: (u32, u32),
    folder: bool,
    children: Vec<u32>,
}

impl Tree {
    /// Builds the tree from NUL-separated relative paths, in any order and
    /// with repeats, as [`crate::vcs::Repository::files`] prints them.
    pub fn from_listing(names: Vec<u8>) -> Self {
        let mut paths: Vec<(u32, u32)> = Vec::new();
        let mut start = 0;
        for (i, byte) in names.iter().enumerate() {
            if *byte == 0 {
                if i > start {
                    paths.push((start as u32, i as u32));
                }
                start = i + 1;
            }
        }
        if start < names.len() {
            paths.push((start as u32, names.len() as u32));
        }
        let text = |(a, b): (u32, u32)| &names[a as usize..b as usize];
        paths.sort_unstable_by(|a, b| text(*a).cmp(text(*b)));
        paths.dedup_by(|a, b| text(*a) == text(*b));

        // Sorted paths keep every folder's entries together, so a stack of
        // open folders is enough to place each one.
        let mut drafts = vec![Draft {
            name: (0, 0),
            folder: true,
            children: Vec::new(),
        }];
        // (name range, draft index) of each open folder below the root.
        let mut open: Vec<((u32, u32), u32)> = Vec::new();
        let mut files = 0;
        for (from, to) in paths {
            let mut components = Vec::new();
            let mut part = from;
            for i in from..to {
                if names[i as usize] == b'/' {
                    if i > part {
                        components.push((part, i));
                    }
                    part = i + 1;
                }
            }
            if part >= to {
                continue;
            }
            let folders = &components[..];
            let shared = open
                .iter()
                .zip(folders)
                .take_while(|((name, _), folder)| text(*name) == text(**folder))
                .count();
            open.truncate(shared);
            for folder in &folders[shared..] {
                let parent = open.last().map_or(0, |(_, id)| *id);
                let id = drafts.len() as u32;
                drafts.push(Draft {
                    name: *folder,
                    folder: true,
                    children: Vec::new(),
                });
                drafts[parent as usize].children.push(id);
                open.push((*folder, id));
            }
            let parent = open.last().map_or(0, |(_, id)| *id);
            let id = drafts.len() as u32;
            drafts.push(Draft {
                name: (part, to),
                folder: false,
                children: Vec::new(),
            });
            drafts[parent as usize].children.push(id);
            files += 1;
        }

        for draft in 0..drafts.len() {
            let mut children = std::mem::take(&mut drafts[draft].children);
            children.sort_by(|a, b| {
                let (a, b) = (&drafts[*a as usize], &drafts[*b as usize]);
                b.folder
                    .cmp(&a.folder)
                    .then_with(|| caseless(text(a.name), text(b.name)))
            });
            drafts[draft].children = children;
        }

        let mut nodes = Vec::with_capacity(drafts.len().saturating_sub(1));
        // Depth-first walk: (draft, parent node, depth).
        let mut stack: Vec<(u32, u32, u16)> = drafts[0]
            .children
            .iter()
            .rev()
            .map(|child| (*child, NO_PARENT, 0))
            .collect();
        let mut ends: Vec<usize> = Vec::new();
        while let Some((draft, parent, depth)) = stack.pop() {
            // Close the subtrees this node is not part of.
            while ends.len() > usize::from(depth) {
                let open = ends.pop().unwrap_or_default();
                let end = nodes.len() as u32;
                let node: &mut Node = &mut nodes[open];
                node.end = end;
            }
            let draft = &drafts[draft as usize];
            let id = nodes.len() as u32;
            nodes.push(Node {
                name: draft.name,
                parent,
                end: id + 1,
                depth,
                folder: draft.folder,
            });
            if draft.folder {
                ends.push(id as usize);
                for child in draft.children.iter().rev() {
                    stack.push((*child, id, depth + 1));
                }
            }
        }
        let end = nodes.len() as u32;
        for open in ends {
            nodes[open].end = end;
        }
        let expanded = vec![false; nodes.len()];
        let changes = vec![0; nodes.len()];
        Self {
            names,
            nodes,
            expanded,
            changes,
            files,
        }
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Number of files, folders not counted.
    pub fn files(&self) -> usize {
        self.files
    }

    pub fn name(&self, id: NodeId) -> &[u8] {
        let (from, to) = self.nodes[id as usize].name;
        &self.names[from as usize..to as usize]
    }

    pub fn depth(&self, id: NodeId) -> usize {
        usize::from(self.nodes[id as usize].depth)
    }

    pub fn is_folder(&self, id: NodeId) -> bool {
        self.nodes[id as usize].folder
    }

    pub fn is_expanded(&self, id: NodeId) -> bool {
        self.expanded[id as usize]
    }

    /// The node's status letter, `None` while it is unchanged.
    pub fn change(&self, id: NodeId) -> Option<u8> {
        let letter = self.changes[id as usize];
        (letter != 0).then_some(letter)
    }

    /// Marks the files of [`crate::vcs::Repository::changes`] records and
    /// the folders above them. Paths the tree lacks are skipped.
    /// Forgets every change mark, before the marks of a new status.
    pub fn clear_changes(&mut self) {
        self.changes.fill(0);
    }

    pub fn mark_changes(&mut self, records: &[u8]) {
        for record in records.split(|&b| b == 0) {
            let [letter, b' ', path @ ..] = record else {
                continue;
            };
            let Some(id) = self.find_bytes(path) else {
                continue;
            };
            self.changes[id as usize] = *letter;
            let mut at = self.parent(id);
            while let Some(folder) = at {
                let mark = &mut self.changes[folder as usize];
                *mark = match *mark {
                    0 => *letter,
                    same if same == *letter => same,
                    _ => b'M',
                };
                at = self.parent(folder);
            }
        }
    }

    /// [`Tree::find`] for a `/`-separated path as the VCS prints it.
    fn find_bytes(&self, path: &[u8]) -> Option<NodeId> {
        let mut range = 0..self.nodes.len();
        let mut found = None;
        for part in path.split(|&b| b == b'/').filter(|part| !part.is_empty()) {
            let mut i = range.start;
            let id = loop {
                if i >= range.end {
                    return None;
                }
                if self.name(i as NodeId) == part {
                    break i;
                }
                i = self.nodes[i].end as usize;
            };
            found = Some(id as NodeId);
            range = id + 1..self.nodes[id].end as usize;
        }
        found
    }

    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        let parent = self.nodes[id as usize].parent;
        (parent != NO_PARENT).then_some(parent)
    }

    /// Expands or collapses a folder. Files ignore it.
    pub fn set_expanded(&mut self, id: NodeId, expanded: bool) {
        if self.is_folder(id) {
            self.expanded[id as usize] = expanded;
        }
    }

    /// Writes the nodes a reader sees, in order, into `out`: every node
    /// whose ancestors are all expanded.
    pub fn visible(&self, out: &mut Vec<NodeId>) {
        out.clear();
        let mut i = 0;
        while i < self.nodes.len() {
            out.push(i as NodeId);
            let node = &self.nodes[i];
            i = if node.folder && !self.expanded[i] {
                node.end as usize
            } else {
                i + 1
            };
        }
    }

    /// The path of `id` relative to the worktree root.
    pub fn path(&self, id: NodeId) -> PathBuf {
        let mut parts = Vec::new();
        let mut at = Some(id);
        while let Some(node) = at {
            parts.push(self.name(node));
            at = self.parent(node);
        }
        let mut path = PathBuf::new();
        for part in parts.iter().rev() {
            path.push(String::from_utf8_lossy(part).as_ref());
        }
        path
    }

    /// The node at the relative `path`, if the tree has it.
    pub fn find(&self, path: &Path) -> Option<NodeId> {
        let mut range = 0..self.nodes.len();
        let mut found = None;
        for part in path.components() {
            let part = part.as_os_str().as_encoded_bytes();
            let mut i = range.start;
            let id = loop {
                if i >= range.end {
                    return None;
                }
                if self.name(i as NodeId) == part {
                    break i;
                }
                i = self.nodes[i].end as usize;
            };
            found = Some(id as NodeId);
            range = id + 1..self.nodes[id].end as usize;
        }
        found
    }

    /// Expands every folder above `id`.
    pub fn reveal(&mut self, id: NodeId) {
        let mut at = self.parent(id);
        while let Some(folder) = at {
            self.expanded[folder as usize] = true;
            at = self.parent(folder);
        }
    }
}

fn caseless(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    a.iter()
        .map(u8::to_ascii_lowercase)
        .cmp(b.iter().map(u8::to_ascii_lowercase))
        .then_with(|| a.cmp(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shown(tree: &Tree) -> Vec<String> {
        let mut ids = Vec::new();
        tree.visible(&mut ids);
        ids.iter()
            .map(|id| {
                let indent = "  ".repeat(tree.depth(*id));
                let mark = if tree.is_folder(*id) { "/" } else { "" };
                format!("{indent}{}{mark}", String::from_utf8_lossy(tree.name(*id)))
            })
            .collect()
    }

    fn sample() -> Tree {
        Tree::from_listing(
            b"src/main.rs\0README\0src/ui/a.rs\0Cargo.toml\0src/lib.rs\0b-c\0src/main.rs\0a.txt\0src/Z.rs\0".to_vec(),
        )
    }

    #[test]
    fn folders_come_first_and_start_collapsed() {
        let tree = sample();
        assert_eq!(tree.files(), 8);
        assert_eq!(
            shown(&tree),
            ["src/", "a.txt", "b-c", "Cargo.toml", "README"]
        );
    }

    #[test]
    fn expanding_shows_children_in_order() {
        let mut tree = sample();
        let src = tree.find(Path::new("src")).unwrap();
        tree.set_expanded(src, true);
        assert_eq!(
            shown(&tree),
            [
                "src/",
                "  ui/",
                "  lib.rs",
                "  main.rs",
                "  Z.rs",
                "a.txt",
                "b-c",
                "Cargo.toml",
                "README"
            ]
        );
        let file = tree.find(Path::new("src/ui/a.rs")).unwrap();
        assert_eq!(tree.path(file), Path::new("src/ui/a.rs"));
        tree.set_expanded(src, false);
        tree.reveal(file);
        assert_eq!(shown(&tree).len(), 10);
        assert_eq!(tree.find(Path::new("src/nope.rs")), None);
    }

    #[test]
    fn changes_mark_files_and_the_folders_above_them() {
        let mut tree = sample();
        tree.mark_changes(b"M src/ui/a.rs\0? src/lib.rs\0A a.txt\0M not/here\0");
        let at = |path: &str| tree.change(tree.find(Path::new(path)).unwrap());
        assert_eq!(at("src/ui/a.rs"), Some(b'M'));
        assert_eq!(at("src/ui"), Some(b'M'));
        // An untracked and a modified file below it: the folder says `M`.
        assert_eq!(at("src"), Some(b'M'));
        assert_eq!(at("src/lib.rs"), Some(b'?'));
        assert_eq!(at("a.txt"), Some(b'A'));
        assert_eq!(at("README"), None);
    }

    #[test]
    fn an_empty_listing_has_no_nodes() {
        let tree = Tree::from_listing(Vec::new());
        assert_eq!(tree.len(), 0);
        let mut ids = vec![7];
        tree.visible(&mut ids);
        assert!(ids.is_empty());
    }
}
