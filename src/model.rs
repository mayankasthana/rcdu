//! The in-memory tree owned by the UI thread.
//!
//! The scanner streams [`Batch`]es of newly discovered entries; [`Tree::apply`] grafts them in
//! and propagates their sizes up every ancestor so totals are always live and correct, even
//! mid-scan. Batches that arrive before their parent node exists (which channel ordering should
//! prevent, but we don't rely on it) are buffered and flushed once the parent appears.
//!
//! Sizes that don't "count" — hard-link duplicates and excluded entries — keep their real own
//! size for display but contribute zero to the aggregated totals, so a tree's grand total
//! dedupes hard links and ignores excluded subtrees, matching ncdu.
//!
//! Memory: there is one [`Node`] per filesystem entry, so the struct is kept lean. Names use
//! `CompactString` (inlined when ≤24 bytes, the common case, so most entries allocate nothing).
//! A node's *own* size is derived from its children rather than stored, and the scan-only
//! `dir_index` map is released once scanning finishes.

use std::collections::HashMap;
use std::path::MAIN_SEPARATOR;

use compact_str::CompactString;

use crate::scan::Batch;

pub type NodeIdx = usize;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Dir,
    File,
    Link,
    /// Anything else: socket, fifo, device, ...
    Other,
}

/// Why an entry was not descended into (and excluded from totals).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Excluded {
    No,
    /// Matched a `--exclude` pattern.
    Pattern,
    /// On a different filesystem and `-x` was given.
    OtherFs,
}

pub struct Node {
    pub name: CompactString,
    pub kind: NodeKind,
    /// Aggregated, *counted* logical size (own entry + everything beneath it that counts).
    pub apparent: u64,
    /// Aggregated, *counted* on-disk size.
    pub disk: u64,
    pub parent: Option<NodeIdx>,
    pub children: Vec<NodeIdx>,
    pub dev: u64,
    pub ino: u64,
    /// Link count > 1 (a hard-link candidate).
    pub hlink: bool,
    /// A hard-link whose inode was already counted elsewhere — excluded from totals.
    pub shared: bool,
    pub excluded: Excluded,
    pub read_error: bool,
    /// True once this directory's own `readdir` has been applied.
    pub scanned: bool,
    /// Number of directories in this subtree (including self) not yet `readdir`-ed.
    /// A subtree is fully scanned — and only then safe to delete — when this hits 0.
    pub unscanned: u32,
}

impl Node {
    pub fn is_dir(&self) -> bool {
        matches!(self.kind, NodeKind::Dir)
    }

    /// Does this entry contribute to aggregated totals?
    pub fn counts(&self) -> bool {
        !self.shared && self.excluded == Excluded::No
    }
}

pub struct Tree {
    pub nodes: Vec<Node>,
    pub root: NodeIdx,
    /// Maps a scanner directory id to its node index. Used only while scanning; released by
    /// [`Tree::finish_scan`].
    dir_index: HashMap<u64, NodeIdx>,
    /// Batches whose parent hasn't been created yet, keyed by `parent_id`.
    orphans: HashMap<u64, Vec<Batch>>,
    pub total_files: u64,
    pub total_dirs: u64,
}

impl Tree {
    /// Build a tree containing only the (directory) root, using its own stat info.
    pub fn new(root_name: CompactString, apparent: u64, disk: u64, dev: u64, ino: u64) -> Self {
        let root = Node {
            name: root_name,
            kind: NodeKind::Dir,
            apparent,
            disk,
            parent: None,
            children: Vec::new(),
            dev,
            ino,
            hlink: false,
            shared: false,
            excluded: Excluded::No,
            read_error: false,
            scanned: false,
            // The root itself is an unscanned directory until its batch arrives.
            unscanned: 1,
        };
        let mut dir_index = HashMap::new();
        dir_index.insert(0, 0);
        Tree {
            nodes: vec![root],
            root: 0,
            dir_index,
            orphans: HashMap::new(),
            total_dirs: 1,
            total_files: 0,
        }
    }

    /// Graft a scanner batch into the tree.
    pub fn apply(&mut self, batch: Batch) {
        let Some(&parent_idx) = self.dir_index.get(&batch.parent_id) else {
            // Parent not created yet — stash and retry later.
            self.orphans.entry(batch.parent_id).or_default().push(batch);
            return;
        };
        self.graft(parent_idx, batch);
    }

    /// Release scan-only bookkeeping once no more batches will arrive. Frees the per-directory
    /// `dir_index` map, which can be a large fraction of memory on directory-heavy trees.
    pub fn finish_scan(&mut self) {
        self.dir_index = HashMap::new();
        self.orphans = HashMap::new();
    }

    fn graft(&mut self, parent_idx: NodeIdx, batch: Batch) {
        if !self.nodes[parent_idx].scanned {
            self.nodes[parent_idx].scanned = true;
            // This directory is now read: one fewer unscanned dir in every ancestor's subtree.
            self.bump_unscanned(parent_idx, -1);
        }
        if batch.error.is_some() {
            self.nodes[parent_idx].read_error = true;
        }

        let mut sum_apparent = 0u64;
        let mut sum_disk = 0u64;
        let mut new_dir_ids = Vec::new();
        let mut new_subdirs = Vec::new();

        // Each dir receives exactly one batch (one readdir), so reserve children exactly to
        // avoid the Vec's growth slack.
        self.nodes[parent_idx]
            .children
            .reserve_exact(batch.nodes.len());

        for nn in batch.nodes {
            let idx = self.nodes.len();
            let is_dir = matches!(nn.kind, NodeKind::Dir);
            if is_dir {
                self.total_dirs += 1;
            } else {
                self.total_files += 1;
            }

            let counts = !nn.shared && nn.excluded == Excluded::No;
            if counts {
                sum_apparent += nn.apparent;
                sum_disk += nn.disk;
            }

            self.nodes.push(Node {
                name: nn.name,
                kind: nn.kind,
                apparent: nn.apparent,
                disk: nn.disk,
                parent: Some(parent_idx),
                children: Vec::new(),
                dev: nn.dev,
                ino: nn.ino,
                hlink: nn.hlink,
                shared: nn.shared,
                excluded: nn.excluded,
                read_error: nn.read_error,
                scanned: false,
                unscanned: 0,
            });
            self.nodes[parent_idx].children.push(idx);

            if let Some(id) = nn.dir_id {
                // A directory we will recurse into: register it and mark it unscanned.
                self.dir_index.insert(id, idx);
                new_dir_ids.push(id);
                new_subdirs.push(idx);
            }
        }

        // Each recursable subdir adds one unscanned dir to itself and every ancestor.
        for idx in new_subdirs {
            self.bump_unscanned(idx, 1);
        }

        // Push the children's combined counted size up through every ancestor.
        self.add_size(parent_idx, sum_apparent, sum_disk);

        // Flush any batches that were waiting on directories we just created.
        for id in new_dir_ids {
            if let Some(waiting) = self.orphans.remove(&id) {
                let child_idx = self.dir_index[&id];
                for b in waiting {
                    self.graft(child_idx, b);
                }
            }
        }
    }

    fn add_size(&mut self, start: NodeIdx, apparent: u64, disk: u64) {
        let mut cur = Some(start);
        while let Some(i) = cur {
            self.nodes[i].apparent += apparent;
            self.nodes[i].disk += disk;
            cur = self.nodes[i].parent;
        }
    }

    fn sub_size(&mut self, start: NodeIdx, apparent: u64, disk: u64) {
        let mut cur = Some(start);
        while let Some(i) = cur {
            self.nodes[i].apparent = self.nodes[i].apparent.saturating_sub(apparent);
            self.nodes[i].disk = self.nodes[i].disk.saturating_sub(disk);
            cur = self.nodes[i].parent;
        }
    }

    fn bump_unscanned(&mut self, start: NodeIdx, delta: i32) {
        let mut cur = Some(start);
        while let Some(i) = cur {
            let v = self.nodes[i].unscanned as i64 + delta as i64;
            self.nodes[i].unscanned = v.max(0) as u32;
            cur = self.nodes[i].parent;
        }
    }

    /// This entry's own size, excluding children — derived rather than stored.
    /// (`apparent` of a node equals its own size plus the counted sizes of its children.)
    pub fn own_apparent(&self, idx: NodeIdx) -> u64 {
        let n = &self.nodes[idx];
        let kids: u64 = n
            .children
            .iter()
            .filter(|&&c| self.nodes[c].counts())
            .map(|&c| self.nodes[c].apparent)
            .sum();
        n.apparent.saturating_sub(kids)
    }

    pub fn own_disk(&self, idx: NodeIdx) -> u64 {
        let n = &self.nodes[idx];
        let kids: u64 = n
            .children
            .iter()
            .filter(|&&c| self.nodes[c].counts())
            .map(|&c| self.nodes[c].disk)
            .sum();
        n.disk.saturating_sub(kids)
    }

    /// True if this entire subtree has been fully scanned (every descendant directory read).
    /// Files are trivially complete. This is the gate for deletion.
    pub fn subtree_complete(&self, idx: NodeIdx) -> bool {
        self.nodes[idx].unscanned == 0
    }

    /// Remove a fully-scanned subtree from the tree (after it has been removed from disk).
    /// Returns the number of (dirs, files) removed.
    pub fn delete_subtree(&mut self, idx: NodeIdx) -> (u64, u64) {
        // Detach from parent and subtract its counted size from all ancestors.
        if let Some(parent) = self.nodes[idx].parent {
            self.nodes[parent].children.retain(|&c| c != idx);
            if self.nodes[idx].counts() {
                let (ap, dk) = (self.nodes[idx].apparent, self.nodes[idx].disk);
                self.sub_size(parent, ap, dk);
            }
        }
        // Walk the subtree to update global counts (nodes become unreachable tombstones).
        let mut dirs = 0u64;
        let mut files = 0u64;
        let mut stack = vec![idx];
        while let Some(n) = stack.pop() {
            if self.nodes[n].is_dir() {
                dirs += 1;
            } else {
                files += 1;
            }
            stack.extend_from_slice(&self.nodes[n].children);
        }
        self.total_dirs = self.total_dirs.saturating_sub(dirs);
        self.total_files = self.total_files.saturating_sub(files);
        (dirs, files)
    }

    /// Prepare a subtree for an in-place rescan (`r`): its children are detached (and dropped
    /// from the global dir/file counts), its aggregates are reset to its own size — so every
    /// ancestor's total stays correct while the rescan streams back in — and the subtree is
    /// marked unscanned again. `id_base` registers the node as the root of a new scan
    /// generation (see `scan::start_at`); batches from that scan graft back into this node.
    pub fn begin_refresh(&mut self, idx: NodeIdx, id_base: u64) {
        debug_assert!(self.nodes[idx].is_dir());
        // Own size first: the aggregate minus the counted children is this node's own size,
        // which is what the reset aggregate becomes.
        let (own_apparent, own_disk) = (self.own_apparent(idx), self.own_disk(idx));

        let mut stack = std::mem::take(&mut self.nodes[idx].children);
        let mut dirs = 0u64;
        let mut files = 0u64;
        while let Some(n) = stack.pop() {
            if self.nodes[n].is_dir() {
                dirs += 1;
            } else {
                files += 1;
            }
            // Detach the grandchildren too: the nodes stay as tombstones (like delete), but
            // they no longer hold on to their subtree.
            let grandkids = std::mem::take(&mut self.nodes[n].children);
            stack.extend_from_slice(&grandkids);
        }
        self.total_dirs = self.total_dirs.saturating_sub(dirs);
        self.total_files = self.total_files.saturating_sub(files);

        let n = &mut self.nodes[idx];
        let removed_apparent = n.apparent.saturating_sub(own_apparent);
        let removed_disk = n.disk.saturating_sub(own_disk);
        let parent = n.parent;
        n.apparent = own_apparent;
        n.disk = own_disk;
        n.scanned = false;
        n.read_error = false;
        n.unscanned = 1;
        // The detached children's counted sizes are baked into every ancestor's aggregate —
        // remove them, so totals stay correct while the rescan streams the subtree back in.
        if let Some(parent) = parent {
            self.sub_size(parent, removed_apparent, removed_disk);
        }
        self.dir_index.insert(id_base, idx);
    }

    /// Build a filesystem path for a node, walking back to the root (whose name is absolute).
    pub fn path_of(&self, idx: NodeIdx) -> String {
        let mut parts = Vec::new();
        let mut cur = Some(idx);
        while let Some(i) = cur {
            parts.push(self.nodes[i].name.as_str());
            cur = self.nodes[i].parent;
        }
        parts.reverse();
        // The root name is already an absolute path; join the rest with separators.
        let mut out = String::new();
        for (i, p) in parts.iter().enumerate() {
            if i > 0 && !out.ends_with(MAIN_SEPARATOR) {
                out.push(MAIN_SEPARATOR);
            }
            out.push_str(p);
        }
        out
    }

    // ---- Import support (see dump.rs) -------------------------------------------------

    /// Create a tree whose root carries imported metadata; it is considered fully scanned.
    pub fn new_imported(name: CompactString, apparent: u64, disk: u64, dev: u64, ino: u64) -> Self {
        let mut t = Tree::new(name, apparent, disk, dev, ino);
        t.nodes[t.root].scanned = true;
        t.nodes[t.root].unscanned = 0;
        t
    }

    /// Append an imported child under `parent`, propagating its counted size. Returns its index.
    #[allow(clippy::too_many_arguments)]
    pub fn import_child(
        &mut self,
        parent: NodeIdx,
        name: CompactString,
        kind: NodeKind,
        apparent: u64,
        disk: u64,
        dev: u64,
        ino: u64,
        hlink: bool,
        shared: bool,
        excluded: Excluded,
        read_error: bool,
    ) -> NodeIdx {
        let idx = self.nodes.len();
        if matches!(kind, NodeKind::Dir) {
            self.total_dirs += 1;
        } else {
            self.total_files += 1;
        }
        self.nodes.push(Node {
            name,
            kind,
            apparent,
            disk,
            parent: Some(parent),
            children: Vec::new(),
            dev,
            ino,
            hlink,
            shared,
            excluded,
            read_error,
            scanned: true,
            unscanned: 0,
        });
        self.nodes[parent].children.push(idx);
        if !shared && excluded == Excluded::No {
            self.add_size(parent, apparent, disk);
        }
        idx
    }

    /// True if no batch is still waiting for a missing parent (used in tests).
    #[cfg(test)]
    pub fn orphans_is_empty(&self) -> bool {
        self.orphans.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::NewNode;

    fn dir(name: &str, id: u64) -> NewNode {
        NewNode {
            name: name.into(),
            kind: NodeKind::Dir,
            apparent: 100,
            disk: 100,
            dev: 1,
            ino: id,
            hlink: false,
            shared: false,
            excluded: Excluded::No,
            read_error: false,
            dir_id: Some(id),
        }
    }

    /// A directory is not deletable until its entire subtree has been scanned.
    #[test]
    fn delete_gate_tracks_unscanned_subtree() {
        let mut t = Tree::new("/r".into(), 100, 100, 1, 1);
        // Root not scanned yet → incomplete.
        assert!(!t.subtree_complete(t.root));

        // Apply root's batch: it declares one subdir `a` (id 1), still unscanned.
        t.apply(Batch {
            parent_id: 0,
            nodes: vec![dir("a", 1)],
            error: None,
        });
        let a = t.nodes[t.root].children[0];
        assert!(
            !t.subtree_complete(t.root),
            "root has an unscanned child dir"
        );
        assert!(!t.subtree_complete(a), "a itself not scanned yet");

        // `a` declares a nested subdir `b` (id 2): still incomplete deeper.
        t.apply(Batch {
            parent_id: 1,
            nodes: vec![dir("b", 2)],
            error: None,
        });
        assert!(!t.subtree_complete(a), "a has an unscanned child b");

        // `b` is empty and now scanned → everything is complete.
        t.apply(Batch {
            parent_id: 2,
            nodes: vec![],
            error: None,
        });
        assert!(t.subtree_complete(t.nodes[a].children[0]), "b complete");
        assert!(t.subtree_complete(a), "a complete");
        assert!(t.subtree_complete(t.root), "root complete");
    }
}
