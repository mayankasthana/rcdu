//! Application state and input handling.
//!
//! Selection tracks a node's *identity*, not its row position, so the highlight stays glued to
//! the same entry even as live size updates reshuffle the sort order during a scan.

use std::io::{BufWriter, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crossbeam_channel::{unbounded, Receiver};

use crate::model::{Excluded, NodeIdx, NodeKind, Tree};
use crate::scan::{start_at, Batch, Opts, Scan, ScanControl, SCAN_ID_STRIDE};

#[derive(Clone, Copy, PartialEq)]
pub enum SortKey {
    Size,
    Name,
}

/// One row of the top-files popup: a file, its path relative to the directory the popup was
/// opened from, and its size in the metric active when the popup was opened.
pub struct TopEntry {
    pub idx: NodeIdx,
    pub path: String,
    pub size: u64,
}

/// State of the top-files popup (largest files under the viewed directory).
pub struct TopFiles {
    pub items: Vec<TopEntry>,
    pub selected: usize,
}

/// How many rows the top-files popup shows.
const TOP_FILES_LIMIT: usize = 100;

/// How long the whole-tree search waits after the last keystroke before it runs.
pub const FIND_DEBOUNCE: Duration = Duration::from_millis(100);
/// The most hits the whole-tree search keeps. Broad queries (`e`) can match half the tree;
/// past the cap, matches are only counted, and the popup asks for a narrower query.
pub const FIND_CAP: usize = 10_000;
/// The shortest interval between re-sorts of the hit list while it grows or sizes change.
const FIND_RESORT: Duration = Duration::from_millis(250);

/// State of the whole-tree search popup (`f`).
///
/// The search is one linear pass over `tree.nodes`, sliced across frames by a cursor so a huge
/// tree never blocks the UI. Nodes are only ever appended, so the same cursor also picks up
/// entries that a running scan or rescan adds later: live results need no special case.
pub struct Find {
    /// The query being typed.
    pub query: String,
    /// The query was edited since the last tick; the next tick (re)arms the debounce.
    edited: bool,
    /// When the debounced query is due to run.
    due: Option<Instant>,
    /// The query the current pass matches against; None while nothing is searched.
    pub active: Option<String>,
    /// Next node the pass will look at.
    pub cursor: NodeIdx,
    /// Matching nodes (at most `FIND_CAP`), sorted for display.
    pub hits: Vec<NodeIdx>,
    /// Every match found, including those past the cap.
    pub total: usize,
    /// Highlighted hit, by identity, so re-sorts and live inserts don't move the highlight.
    pub selected: Option<NodeIdx>,
    sort_dirty: bool,
    last_sort: Option<Instant>,
}

impl Find {
    fn new() -> Self {
        Find {
            query: String::new(),
            edited: false,
            due: None,
            active: None,
            cursor: 0,
            hits: Vec::new(),
            total: 0,
            selected: None,
            sort_dirty: false,
            last_sort: None,
        }
    }

    /// True while the pass has nodes left to look at.
    pub fn busy(&self, tree: &Tree) -> bool {
        self.active.is_some() && self.cursor < tree.nodes.len()
    }

    /// True once more matches were found than the list keeps.
    pub fn capped(&self) -> bool {
        self.total > self.hits.len()
    }

    /// Start a fresh pass over the whole tree for the active query.
    fn restart(&mut self) {
        self.cursor = 0;
        self.hits.clear();
        self.total = 0;
        self.selected = None;
        self.sort_dirty = false;
    }

    /// One frame's worth of work: debounce, then up to `budget` nodes of the pass, then a
    /// (throttled) re-sort. `now` is passed in so tests can drive the clock.
    fn tick(&mut self, tree: &Tree, disk_usage: bool, scanning: bool, now: Instant, budget: usize) {
        if self.edited {
            self.edited = false;
            self.due = Some(now + FIND_DEBOUNCE);
        }
        if self.due.is_some_and(|d| now >= d) {
            self.due = None;
            self.active = (!self.query.is_empty()).then(|| self.query.clone());
            self.restart();
        }
        let Some(q) = &self.active else { return };

        let len = tree.nodes.len();
        let was_busy = self.cursor < len;
        let end = len.min(self.cursor.saturating_add(budget));
        // Tombstones exist only after a delete or rescan; until then skip the parent walk.
        let check_attached = tree.detached > 0;
        for i in self.cursor..end {
            if i != tree.root
                && contains_ci(&tree.nodes[i].name, q)
                && (!check_attached || tree.is_attached(i))
            {
                self.total += 1;
                if self.hits.len() < FIND_CAP {
                    self.hits.push(i);
                    self.sort_dirty = true;
                }
            }
        }
        self.cursor = end;
        let finished = was_busy && end == len;

        // Sizes keep changing while a scan runs, so the order goes stale even without new hits.
        let stale = self.sort_dirty || (scanning && !self.hits.is_empty());
        let throttled = self
            .last_sort
            .is_some_and(|t| now.saturating_duration_since(t) < FIND_RESORT);
        if stale && (finished || !throttled) {
            let size = |i: NodeIdx| {
                let n = &tree.nodes[i];
                if disk_usage {
                    n.disk
                } else {
                    n.apparent
                }
            };
            self.hits.sort_by(|&a, &b| {
                size(b)
                    .cmp(&size(a))
                    .then_with(|| tree.nodes[a].name.cmp(&tree.nodes[b].name))
                    .then_with(|| a.cmp(&b))
            });
            self.sort_dirty = false;
            self.last_sort = Some(now);
        }
        self.ensure_selection();
    }

    /// Drop hits that a delete or rescan turned into tombstones. Past the cap the true count
    /// can't be corrected from the kept hits alone, so the pass starts over instead.
    fn prune(&mut self, tree: &Tree) {
        if self.active.is_none() {
            return;
        }
        if self.capped() {
            self.restart();
            return;
        }
        let before = self.hits.len();
        self.hits.retain(|&i| tree.is_attached(i));
        self.total -= before - self.hits.len();
        self.ensure_selection();
    }

    fn ensure_selection(&mut self) {
        match self.selected {
            Some(s) if self.hits.contains(&s) => {}
            _ => self.selected = self.hits.first().copied(),
        }
    }

    /// Position of the highlighted hit in the list.
    pub fn selected_pos(&self) -> Option<usize> {
        self.selected
            .and_then(|s| self.hits.iter().position(|&h| h == s))
    }

    fn move_selection(&mut self, delta: isize) {
        if self.hits.is_empty() {
            return;
        }
        let pos = self.selected_pos().unwrap_or(0) as isize;
        let new = (pos + delta).clamp(0, self.hits.len() as isize - 1) as usize;
        self.selected = Some(self.hits[new]);
    }
}

/// How raw keys should be decoded right now.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InputMode {
    Normal,
    /// Typing the `/` filter.
    Filter,
    /// The whole-tree search popup is open: letters go to its query.
    Find,
}

/// A transient modal overlay.
pub enum Modal {
    None,
    Help,
    /// Confirm deletion of the given node.
    ConfirmDelete(NodeIdx),
    /// The largest files under the directory the popup was opened from.
    TopFiles(Box<TopFiles>),
    /// Details of the selected entry (tree data plus a fresh on-disk stat).
    Info(NodeIdx),
    /// Whole-tree search by name.
    Find(Box<Find>),
}

/// An in-progress deletion running on a background thread, so the UI stays responsive and shows
/// a spinner while a large directory is removed.
struct PendingDelete {
    idx: NodeIdx,
    path: String,
    rx: Receiver<std::io::Result<()>>,
}

/// A system-open running on a background thread, so a slow or hung opener can't freeze the UI.
struct PendingOpen {
    path: String,
    rx: Receiver<std::io::Result<()>>,
}

/// One live batch stream: the original scan (`refresh: false`) or a subtree rescan
/// (`refresh: true`). `alive` flips false once the stream's channel disconnects.
struct ScanHandle {
    rx: Receiver<Batch>,
    refresh: bool,
    alive: bool,
}

pub struct App {
    pub tree: Tree,
    /// Directory currently being viewed.
    pub cur: NodeIdx,
    /// Currently highlighted child node (by identity), if any.
    pub selected: Option<NodeIdx>,
    pub sort: SortKey,
    /// Incremental name filter (`/`): `Some(query)` while a filter is applied — possibly empty
    /// while still typing — `None` when nothing is filtered.
    pub filter: Option<String>,
    /// True while the user is typing the filter; input is captured for it.
    pub searching: bool,
    /// Selection to restore if the filter being typed is cancelled.
    pre_search_selected: Option<NodeIdx>,
    /// Show on-disk usage (`st_blocks`) vs. apparent size (`st_size`).
    pub disk_usage: bool,
    pub scanning: bool,
    /// Read-only: deletion disabled (ncdu `-r`).
    pub read_only: bool,
    pub quit: bool,
    pub modal: Modal,
    /// Transient one-line message shown in the footer until the next key press.
    pub status: Option<String>,
    /// A deletion currently running on a background thread.
    pending_delete: Option<PendingDelete>,
    /// Opens still running on background threads, oldest first.
    pending_open: Vec<PendingOpen>,
    /// True once the user has pressed quit during a deletion; a second press force-quits.
    quit_armed: bool,
    /// Batch streams of every live scan: the original one plus any subtree rescans.
    scans: Vec<ScanHandle>,
    /// Steering handles for every live scan; navigation re-prioritizes each one.
    controls: Vec<ScanControl>,
    /// Scan options, kept so a subtree rescan can reuse them (same threads, excludes, `-x`).
    /// None when browsing a loaded dump (`-f`), where there is nothing to rescan.
    scan_opts: Option<Opts>,
    /// The subtree currently being rescanned in place, if any.
    pub refreshing_idx: Option<NodeIdx>,
    /// Generation counter for subtree rescans; each gets a disjoint scanner-id range.
    next_scan_base: u64,
    /// Counted bytes grafted by live scans, for the footer's throughput display.
    bytes_seen: u64,
    /// When the current scan generation started; None until a scan is attached.
    scan_started: Option<Instant>,
    /// A pending request to suspend the TUI and spawn `$SHELL` in this directory, consumed
    /// once per frame by the event loop (which owns the terminal).
    shell_request: Option<String>,
    /// Spinner animation frame.
    pub tick: usize,
}

impl App {
    pub fn new(tree: Tree, disk_usage: bool, read_only: bool, scanning: bool) -> Self {
        let mut tree = tree;
        if !scanning {
            // An imported dump arrives complete: release the scan-only bookkeeping up front.
            tree.finish_scan();
        }
        App {
            cur: tree.root,
            selected: None,
            sort: SortKey::Size,
            filter: None,
            searching: false,
            pre_search_selected: None,
            disk_usage,
            scanning,
            read_only,
            quit: false,
            modal: Modal::None,
            status: None,
            pending_delete: None,
            pending_open: Vec::new(),
            quit_armed: false,
            scans: Vec::new(),
            controls: Vec::new(),
            scan_opts: None,
            refreshing_idx: None,
            next_scan_base: 1,
            bytes_seen: 0,
            scan_started: None,
            shell_request: None,
            tick: 0,
            tree,
        }
    }

    /// Attach the initial scan: its batch stream, its steering handle, and the options future
    /// subtree rescans should reuse (interactive scan mode only).
    pub fn attach_scan(&mut self, scan: Scan, opts: Opts) {
        self.scans.push(ScanHandle {
            rx: scan.events,
            refresh: false,
            alive: true,
        });
        self.controls.push(scan.control);
        self.scan_opts = Some(opts);
        self.scan_started = Some(Instant::now());
        self.refocus();
    }

    /// Tell every live scan to prioritize the directory currently being viewed.
    fn refocus(&mut self) {
        if !self.scanning {
            return;
        }
        let focus = Some(PathBuf::from(self.tree.path_of(self.cur)));
        for ctrl in &self.controls {
            ctrl.set_focus(focus.clone());
        }
    }

    /// Drain every live scan's batch stream into the tree (up to `cap` batches per frame, so a
    /// huge filesystem can't starve input handling). Marks streams dead when their channel
    /// disconnects; when a rescan's stream ends the spinner clears, and when the last stream
    /// ends the scan is over and the scan-only bookkeeping is released.
    pub fn poll_scans(&mut self, cap: usize) {
        let mut applied = 0usize;
        for i in 0..self.scans.len() {
            if !self.scans[i].alive {
                continue;
            }
            loop {
                match self.scans[i].rx.try_recv() {
                    Ok(batch) => {
                        // Count the batch's counted bytes before `apply` moves it, for the
                        // footer's throughput display.
                        self.bytes_seen += batch
                            .nodes
                            .iter()
                            .filter(|n| !n.shared && n.excluded == Excluded::No)
                            .map(|n| n.apparent)
                            .sum::<u64>();
                        self.tree.apply(batch);
                        applied += 1;
                        if applied >= cap {
                            break;
                        }
                    }
                    Err(crossbeam_channel::TryRecvError::Empty) => break,
                    Err(crossbeam_channel::TryRecvError::Disconnected) => {
                        self.scans[i].alive = false;
                        break;
                    }
                }
            }
        }

        if self.refreshing_idx.is_some() && !self.scans.iter().any(|h| h.alive && h.refresh) {
            let idx = self.refreshing_idx.take().unwrap();
            self.status = Some(format!("rescanned {}", self.tree.path_of(idx)));
        }
        let any_alive = self.scans.iter().any(|h| h.alive);
        if self.scanning && !any_alive {
            self.scanning = false;
            self.tree.finish_scan();
        }
    }

    pub fn is_deleting(&self) -> bool {
        self.pending_delete.is_some()
    }

    /// True if a deletion in progress covers `sel` (the node itself or an ancestor of it).
    fn deletion_covers(&self, sel: NodeIdx) -> bool {
        self.deleting_idx().is_some_and(|d| {
            let mut cur = Some(sel);
            while let Some(i) = cur {
                if i == d {
                    return true;
                }
                cur = self.tree.nodes[i].parent;
            }
            false
        })
    }

    /// Rescan the selected directory in place: the model tears its subtree down and a new
    /// scanner generation re-reads it from disk, grafting back into the same node. Browsing
    /// stays live the whole time; totals of surrounding directories stay correct throughout.
    fn request_refresh(&mut self) {
        let Some(sel) = self.selected else { return };
        let Some(opts) = self.scan_opts.as_ref() else {
            self.status = Some("rescan unavailable: browsing an imported dump".into());
            return;
        };
        if self.refreshing_idx.is_some() {
            self.status = Some("a rescan is already in progress — please wait".into());
            return;
        }
        if !self.tree.nodes[sel].is_dir() {
            self.status = Some("select a directory to rescan".into());
            return;
        }
        // Scanning a subtree that is being removed from disk would race the deletion.
        if self.deletion_covers(sel) {
            self.status = Some("can't rescan: a deletion is in progress here".into());
            return;
        }
        // The subtree must be fully known before tearing it down — otherwise the original
        // scan could still deliver batches for it and double-count.
        if !self.tree.subtree_complete(sel) {
            self.status = Some("cannot rescan: directory is still being scanned".into());
            return;
        }
        let id_base = self.next_scan_base * SCAN_ID_STRIDE;
        self.next_scan_base += 1;
        let path = self.tree.path_of(sel);
        self.tree.begin_refresh(sel, id_base);
        let scan = start_at(PathBuf::from(&path), opts.clone(), id_base);
        self.prune_find();
        self.scans.push(ScanHandle {
            rx: scan.events,
            refresh: true,
            alive: true,
        });
        self.controls.push(scan.control);
        self.refreshing_idx = Some(sel);
        self.scanning = true;
        // A fresh generation: the throughput display reflects this rescan, not the bytes the
        // original scan already brought in.
        self.bytes_seen = 0;
        self.scan_started = Some(Instant::now());
        self.status = Some(format!("rescanning {path}…"));
    }

    /// Counted bytes per second being grafted by the live scan, for the footer. None when no
    /// scan is running; the first second reports the bytes seen so far.
    pub fn scan_rate(&self) -> Option<u64> {
        if !self.scanning {
            return None;
        }
        let started = self.scan_started?;
        Some(self.bytes_seen / started.elapsed().as_secs().max(1))
    }

    /// Export the whole tree as it is right now (including deletions made this session) to the
    /// next free `rcdu-dump*.json` in the working directory, in the same ncdu format as `-o`.
    /// Synchronous: dumps write fast relative to scanning, and the outcome lands in the
    /// status line.
    fn request_export(&mut self) {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let path = next_export_path(&cwd);
        match self.export_tree_to(&path) {
            Ok(()) => {
                let entries = self.tree.total_dirs + self.tree.total_files;
                self.status = Some(format!("exported {entries} entries to {}", path.display()));
            }
            Err(e) => self.status = Some(format!("export failed: {e}")),
        }
    }

    /// Write the tree as ncdu-compatible JSON (the `-o` format, via `dump::export`).
    fn export_tree_to(&self, path: &Path) -> std::io::Result<()> {
        let mut w = BufWriter::new(std::fs::File::create(path)?);
        crate::dump::export(&self.tree, &mut w)?;
        w.flush()
    }

    /// The aggregated size metric we currently sort and display by.
    pub fn size_of(&self, idx: NodeIdx) -> u64 {
        let n = &self.tree.nodes[idx];
        if self.disk_usage {
            n.disk
        } else {
            n.apparent
        }
    }

    /// The node's own (non-recursive) size in the current metric.
    pub fn own_size_of(&self, idx: NodeIdx) -> u64 {
        if self.disk_usage {
            self.tree.own_disk(idx)
        } else {
            self.tree.own_apparent(idx)
        }
    }

    /// Children of the current directory, sorted for display (largest first, or by name), with
    /// the active name filter (if any) applied on top.
    pub fn sorted_children(&self) -> Vec<NodeIdx> {
        let mut kids = self.order_children(self.tree.nodes[self.cur].children.clone());
        if let Some(q) = &self.filter {
            kids.retain(|&k| contains_ci(&self.tree.nodes[k].name, q));
        }
        kids
    }

    /// Sort children the way every view presents them (largest first, or by name), so
    /// traversals like the next-error jump match what the user sees on screen.
    fn order_children(&self, mut kids: Vec<NodeIdx>) -> Vec<NodeIdx> {
        match self.sort {
            SortKey::Size => kids.sort_by(|&a, &b| {
                self.size_of(b)
                    .cmp(&self.size_of(a))
                    .then_with(|| self.tree.nodes[a].name.cmp(&self.tree.nodes[b].name))
            }),
            SortKey::Name => {
                kids.sort_by(|&a, &b| self.tree.nodes[a].name.cmp(&self.tree.nodes[b].name))
            }
        }
        kids
    }

    /// Jump to the next entry flagged with a read error (`!`), searching forward from the
    /// current selection in display order and wrapping around. The view moves to the entry's
    /// parent directory with it selected.
    fn jump_to_next_error(&mut self) {
        let mut order = Vec::new();
        let mut stack = vec![self.tree.root];
        while let Some(idx) = stack.pop() {
            order.push(idx);
            // Pushed reversed so the pop order is display order.
            stack.extend(
                self.order_children(self.tree.nodes[idx].children.clone())
                    .iter()
                    .rev(),
            );
        }
        let anchor = self.selected.unwrap_or(self.cur);
        let pos = order.iter().position(|&i| i == anchor).unwrap_or(0);
        for step in 1..=order.len() {
            let idx = order[(pos + step) % order.len()];
            if !self.tree.nodes[idx].read_error {
                continue;
            }
            if idx == self.tree.root {
                self.cur = self.tree.root;
                self.selected = None;
            } else {
                self.cur = self.tree.nodes[idx].parent.unwrap();
                self.selected = Some(idx);
            }
            self.refocus();
            self.status = Some(format!("read error: {}", self.tree.path_of(idx)));
            return;
        }
        self.status = Some("no read errors".into());
    }

    /// Request a shell in the selected directory (or, for a file/symlink, the directory
    /// containing it). The event loop owns the terminal, so it consumes the request and
    /// handles the suspend/resume; nothing happens here but recording it.
    fn request_shell(&mut self) {
        let idx = self.selected.unwrap_or(self.cur);
        let target = if self.tree.nodes[idx].is_dir() {
            idx
        } else {
            self.cur
        };
        self.shell_request = Some(self.tree.path_of(target));
    }

    /// Consume a pending shell request, if any.
    pub fn take_shell_request(&mut self) -> Option<String> {
        self.shell_request.take()
    }

    fn ensure_selection(&mut self, kids: &[NodeIdx]) {
        match self.selected {
            Some(sel) if kids.contains(&sel) => {}
            _ => self.selected = kids.first().copied(),
        }
    }

    /// While typing a filter, keep the selection on the first entry matching it.
    fn snap_to_first_match(&mut self) {
        let kids = self.sorted_children();
        self.selected = kids.first().copied();
    }

    /// Open the top-files popup for the directory currently being viewed.
    fn open_top_files(&mut self) {
        let items = self.top_files(self.cur, TOP_FILES_LIMIT);
        if items.is_empty() {
            self.status = Some("no files under this directory".into());
            return;
        }
        self.modal = Modal::TopFiles(Box::new(TopFiles { items, selected: 0 }));
    }

    /// The largest regular files under `from`, largest first, with paths relative to it.
    /// Computed on demand (a subtree walk) so it costs nothing during the scan; entries are a
    /// snapshot of the current sizes, which is exactly what the popup shows.
    pub fn top_files(&self, from: NodeIdx, limit: usize) -> Vec<TopEntry> {
        let mut out: Vec<TopEntry> = Vec::new();
        let mut stack = vec![(from, String::new())];
        while let Some((dir, prefix)) = stack.pop() {
            for &c in &self.tree.nodes[dir].children {
                let n = &self.tree.nodes[c];
                let path = if prefix.is_empty() {
                    n.name.to_string()
                } else {
                    format!("{prefix}/{}", n.name)
                };
                match n.kind {
                    NodeKind::Dir => stack.push((c, path)),
                    NodeKind::File => out.push(TopEntry {
                        idx: c,
                        path,
                        size: self.size_of(c),
                    }),
                    // Symlinks and special files are not "biggest file" material.
                    _ => {}
                }
            }
        }
        out.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.path.cmp(&b.path)));
        out.truncate(limit);
        out
    }

    /// Open the info popup for the selected entry.
    fn open_info(&mut self) {
        if let Some(sel) = self.selected {
            self.modal = Modal::Info(sel);
        }
    }

    /// Rows for the info popup: tree data plus a lazily-statted on-disk section. The stat is
    /// taken at open time and nothing is stored in the node, keeping the tree lean; if the
    /// path is gone (deleted mid-session, or a dump whose paths no longer exist) only the
    /// tree data is shown.
    pub fn info_pairs(&self, idx: NodeIdx) -> Vec<(String, String)> {
        use crate::ui::fmt_size;
        let n = &self.tree.nodes[idx];
        let kind_name = match n.kind {
            NodeKind::Dir => "directory",
            NodeKind::File => "file",
            NodeKind::Link => "symlink",
            NodeKind::Other => "special",
        };
        let mut rows = vec![
            ("path".to_string(), self.tree.path_of(idx)),
            ("type".to_string(), kind_name.to_string()),
            (
                "total size".to_string(),
                fmt_size(self.size_of(idx)).trim_start().to_string(),
            ),
            (
                "own size".to_string(),
                fmt_size(self.own_size_of(idx)).trim_start().to_string(),
            ),
        ];
        if n.is_dir() {
            rows.push(("entries".to_string(), n.children.len().to_string()));
        }
        if n.hlink {
            rows.push(("hard links".to_string(), "yes".to_string()));
        }
        if n.shared {
            rows.push((
                "duplicate".to_string(),
                "hard link already counted elsewhere".to_string(),
            ));
        }
        match n.excluded {
            Excluded::Pattern => {
                rows.push((
                    "excluded".to_string(),
                    "matched --exclude pattern".to_string(),
                ));
            }
            Excluded::OtherFs => {
                rows.push((
                    "excluded".to_string(),
                    "different filesystem (-x)".to_string(),
                ));
            }
            Excluded::Age => {
                rows.push((
                    "excluded".to_string(),
                    "outside --older-than/--newer-than window".to_string(),
                ));
            }
            Excluded::No => {}
        }
        if n.read_error {
            rows.push(("read error".to_string(), "yes".to_string()));
        }
        match std::fs::symlink_metadata(self.tree.path_of(idx)) {
            Ok(m) => {
                rows.push((
                    "permissions".to_string(),
                    format!("{:o}", m.mode() & 0o7777),
                ));
                rows.push(("owner".to_string(), format!("{}:{}", m.uid(), m.gid())));
                rows.push(("modified".to_string(), fmt_time(m.mtime())));
                rows.push(("inode".to_string(), m.ino().to_string()));
                rows.push(("links".to_string(), m.nlink().to_string()));
            }
            Err(_) => rows.push((
                "on disk".to_string(),
                "unavailable (deleted since scanning, or dump paths are gone)".to_string(),
            )),
        }
        rows
    }

    /// The node currently being deleted (its subtree is locked), if any.
    pub fn deleting_idx(&self) -> Option<NodeIdx> {
        self.pending_delete.as_ref().map(|p| p.idx)
    }

    /// How the event loop should decode the next key.
    pub fn input_mode(&self) -> InputMode {
        if matches!(self.modal, Modal::Find(_)) {
            InputMode::Find
        } else if self.searching {
            InputMode::Filter
        } else {
            InputMode::Normal
        }
    }

    /// Advance the whole-tree search, if its popup is open: debounce the query, look at up to
    /// `budget` nodes, and re-sort the hits now and then. Called once per frame, after the
    /// scan batches are applied, so live results show up in the same frame.
    pub fn find_tick(&mut self, now: Instant, budget: usize) {
        if let Modal::Find(f) = &mut self.modal {
            f.tick(&self.tree, self.disk_usage, self.scanning, now, budget);
        }
    }

    /// True while the search pass has work left; the event loop then polls input without
    /// waiting, so the pass finishes as fast as the slicing allows.
    pub fn find_busy(&self) -> bool {
        matches!(&self.modal, Modal::Find(f) if f.busy(&self.tree))
    }

    /// When the pending (debounced) search query is due, if one is waiting.
    pub fn find_due(&self) -> Option<Instant> {
        match &self.modal {
            Modal::Find(f) => f.due,
            _ => None,
        }
    }

    /// Drop search hits that a delete or rescan just detached from the tree.
    fn prune_find(&mut self) {
        if let Modal::Find(f) = &mut self.modal {
            f.prune(&self.tree);
        }
    }

    /// Move the view to a search hit: its parent directory, with the hit selected.
    fn jump_to_hit(&mut self, idx: NodeIdx) {
        if !self.tree.is_attached(idx) {
            self.status = Some("that entry is no longer in the tree".into());
        } else if self.deletion_covers(idx) {
            self.status = Some("can't go there: it is being deleted".into());
        } else if let Some(parent) = self.tree.nodes[idx].parent {
            self.cur = parent;
            self.selected = Some(idx);
            self.refocus();
        }
    }

    pub fn on_key(&mut self, action: KeyAction) {
        // Note: a background deletion does NOT block input — the user can keep browsing. Only
        // the subtree being deleted is locked (see `descend` and `request_delete`).

        // Modal overlays capture input first. The modal is taken out (popups carry state), so
        // every arm that keeps it open must put it back.
        match std::mem::replace(&mut self.modal, Modal::None) {
            Modal::Help => return,
            Modal::ConfirmDelete(idx) => {
                match action {
                    KeyAction::Confirm => self.perform_delete(idx),
                    KeyAction::Quit | KeyAction::Leave | KeyAction::Cancel => {
                        self.status = Some("delete cancelled".into());
                    }
                    // Any other key: keep waiting in the dialog.
                    _ => self.modal = Modal::ConfirmDelete(idx),
                }
                return;
            }
            Modal::TopFiles(mut tf) => {
                let last = tf.items.len().saturating_sub(1);
                match action {
                    KeyAction::Down => tf.selected = (tf.selected + 1).min(last),
                    KeyAction::Up => tf.selected = tf.selected.saturating_sub(1),
                    KeyAction::Top => tf.selected = 0,
                    KeyAction::Bottom => tf.selected = last,
                    KeyAction::Enter => {
                        let idx = tf.items[tf.selected].idx;
                        // The tree may have changed since the popup opened (deletions and
                        // rescans tombstone nodes): only jump if the entry is still attached.
                        if self.tree.is_attached(idx) {
                            self.cur = self.tree.nodes[idx].parent.unwrap();
                            self.selected = Some(idx);
                            self.refocus();
                        } else {
                            self.status = Some("that entry is no longer in the tree".into());
                        }
                    }
                    // q / Esc / h / ? close the popup without changing the view.
                    KeyAction::Quit | KeyAction::Leave | KeyAction::Cancel | KeyAction::Help => {}
                    // Anything else: keep the popup open exactly as it was.
                    _ => self.modal = Modal::TopFiles(tf),
                }
                return;
            }
            // Any key closes the info popup (it's read-only), including q — which must not
            // also quit, so it returns before the quit handling below.
            Modal::Info(_) => return,
            Modal::Find(mut f) => {
                // Whether the popup stays open after this key.
                let keep_open = match action {
                    KeyAction::FindChar(c) => {
                        f.query.push(c);
                        f.edited = true;
                        true
                    }
                    KeyAction::FindBackspace => {
                        f.query.pop();
                        f.edited = true;
                        true
                    }
                    KeyAction::Down => {
                        f.move_selection(1);
                        true
                    }
                    KeyAction::Up => {
                        f.move_selection(-1);
                        true
                    }
                    KeyAction::PageDown(page) => {
                        f.move_selection(page as isize);
                        true
                    }
                    KeyAction::PageUp(page) => {
                        f.move_selection(-(page as isize));
                        true
                    }
                    KeyAction::Top => {
                        f.selected = f.hits.first().copied();
                        true
                    }
                    KeyAction::Bottom => {
                        f.selected = f.hits.last().copied();
                        true
                    }
                    KeyAction::Enter => match f.selected {
                        Some(idx) => {
                            self.jump_to_hit(idx);
                            false
                        }
                        // Nothing to jump to yet.
                        None => true,
                    },
                    // Esc closes the popup and leaves the view as it was. Ctrl-C closes it
                    // too and goes on to the regular quit handling below.
                    KeyAction::Cancel | KeyAction::Quit => false,
                    _ => true,
                };
                if keep_open {
                    self.modal = Modal::Find(f);
                }
                if !matches!(action, KeyAction::Quit) {
                    return;
                }
            }
            Modal::None => {}
        }

        // While typing a filter, capture input: characters extend the query, Backspace erases,
        // Enter applies it, Esc discards it and restores the prior selection. Navigation keys
        // are inert meanwhile (they'd otherwise act on half-typed queries).
        if self.searching {
            match action {
                KeyAction::FilterChar(c) => {
                    if let Some(q) = &mut self.filter {
                        q.push(c);
                    }
                    self.snap_to_first_match();
                }
                KeyAction::FilterBackspace => {
                    if let Some(q) = &mut self.filter {
                        q.pop();
                    }
                    self.snap_to_first_match();
                }
                KeyAction::FilterConfirm => {
                    self.searching = false;
                    if self.filter.as_ref().is_none_or(|q| q.is_empty()) {
                        self.filter = None;
                    }
                }
                KeyAction::FilterCancel => {
                    self.searching = false;
                    self.filter = None;
                    self.selected = self.pre_search_selected.take();
                    self.ensure_selection(&self.sorted_children());
                }
                _ => {}
            }
            return;
        }

        // Any key other than a repeated quit dismisses a stale status and disarms a pending
        // force-quit (so the two-press abort only triggers on consecutive quits).
        if !matches!(action, KeyAction::Quit) {
            self.status = None;
            self.quit_armed = false;
        }

        let kids = self.sorted_children();
        self.ensure_selection(&kids);
        match action {
            KeyAction::Quit => {
                if self.is_deleting() {
                    // Don't silently abort a destructive operation. Arm on the first press,
                    // force-quit on the second (the removal is interrupted and may leave
                    // partial results — remove_dir_all is not cleanly cancellable).
                    if self.quit_armed {
                        self.quit = true;
                    } else {
                        self.quit_armed = true;
                        self.status = Some(
                            "deletion in progress — press q / Ctrl-C again to abort it and quit (may leave partial results)"
                                .into(),
                        );
                    }
                } else {
                    self.quit = true;
                }
            }
            KeyAction::Down => self.move_selection(&kids, 1),
            KeyAction::Up => self.move_selection(&kids, -1),
            KeyAction::PageDown(page) => self.move_selection(&kids, page as isize),
            KeyAction::PageUp(page) => self.move_selection(&kids, -(page as isize)),
            KeyAction::Top => self.selected = kids.first().copied(),
            KeyAction::Bottom => self.selected = kids.last().copied(),
            KeyAction::Enter => self.descend(),
            KeyAction::Leave => self.ascend(),
            KeyAction::ToggleSort => {
                self.sort = match self.sort {
                    SortKey::Size => SortKey::Name,
                    SortKey::Name => SortKey::Size,
                };
            }
            KeyAction::Search => {
                self.searching = true;
                self.status = None;
                self.pre_search_selected = self.selected;
                if self.filter.is_none() {
                    self.filter = Some(String::new());
                }
            }
            KeyAction::ToggleUsage => self.disk_usage = !self.disk_usage,
            KeyAction::Help => self.modal = Modal::Help,
            KeyAction::Delete => self.request_delete(),
            KeyAction::Open => self.open_selected(),
            KeyAction::TopFiles => self.open_top_files(),
            KeyAction::Refresh => self.request_refresh(),
            KeyAction::Info => self.open_info(),
            KeyAction::Export => self.request_export(),
            KeyAction::NextError => self.jump_to_next_error(),
            KeyAction::Shell => self.request_shell(),
            KeyAction::Find => self.modal = Modal::Find(Box::new(Find::new())),
            KeyAction::Confirm | KeyAction::Cancel => {}
            // Only produced while the search popup is open, which handles them above.
            KeyAction::FindChar(_) | KeyAction::FindBackspace => {}
            // Handled by the search-mode block above; unreachable here.
            KeyAction::FilterChar(_)
            | KeyAction::FilterBackspace
            | KeyAction::FilterConfirm
            | KeyAction::FilterCancel => {}
        }
    }

    /// Move the selection by `delta` rows, clamped to the list. `delta` is the page size for
    /// the page keys and ±1 for plain up/down.
    fn move_selection(&mut self, kids: &[NodeIdx], delta: isize) {
        if kids.is_empty() {
            return;
        }
        let pos = self
            .selected
            .and_then(|s| kids.iter().position(|&k| k == s))
            .unwrap_or(0) as isize;
        let new = (pos + delta).clamp(0, kids.len() as isize - 1) as usize;
        self.selected = Some(kids[new]);
    }

    fn descend(&mut self) {
        if let Some(sel) = self.selected {
            // The subtree being deleted is locked: refuse to enter it.
            if self.deleting_idx() == Some(sel) {
                self.status = Some("can't enter: this directory is being deleted".into());
                return;
            }
            if self.tree.nodes[sel].is_dir() {
                self.cur = sel;
                self.selected = self.sorted_children().first().copied();
                // Prioritize scanning the directory we just entered.
                self.refocus();
            }
        }
    }

    fn ascend(&mut self) {
        if let Some(parent) = self.tree.nodes[self.cur].parent {
            let was = self.cur;
            self.cur = parent;
            // Re-select the directory we just came out of.
            self.selected = Some(was);
            self.refocus();
        }
    }

    /// Hand the selected entry to the platform's default handler: files open in their default
    /// app, directories in the file manager. Non-destructive, so unlike delete it also works
    /// in read-only mode and while a scan is still filling in the tree.
    fn open_selected(&mut self) {
        let Some(sel) = self.selected else { return };
        if self.deleting_idx() == Some(sel) {
            self.status = Some("can't open: this entry is being deleted".into());
            return;
        }
        let path = self.tree.path_of(sel);
        let (tx, rx) = unbounded();
        let p = path.clone();
        // Waiting happens off the UI thread: openers normally return at once, but a wedged
        // handler must not take the TUI down with it. Waiting here still reaps the child.
        std::thread::spawn(move || {
            let _ = tx.send(open_path(&p));
        });
        self.status = Some(format!("opening {path}"));
        self.pending_open.push(PendingOpen { path, rx });
    }

    /// Check whether a background open has finished and surface its result. Called once per
    /// frame by the event loop.
    pub fn poll_open(&mut self) {
        if self.pending_open.is_empty() {
            return;
        }
        let mut finished = Vec::new();
        self.pending_open.retain_mut(|p| match p.rx.try_recv() {
            Ok(res) => {
                finished.push((p.path.clone(), res));
                false
            }
            Err(crossbeam_channel::TryRecvError::Empty) => true, // still working
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                finished.push((
                    p.path.clone(),
                    Err(std::io::Error::other("open worker exited unexpectedly")),
                ));
                false
            }
        });
        for (path, res) in finished {
            self.status = Some(match res {
                Ok(()) => format!("opened {path}"),
                Err(e) => format!("open failed: {e}"),
            });
        }
    }

    fn request_delete(&mut self) {
        if self.read_only {
            self.status = Some("read-only mode (-r): deletion disabled".into());
            return;
        }
        // One deletion at a time keeps the locking simple; browsing stays available meanwhile.
        if self.pending_delete.is_some() {
            self.status = Some("a deletion is already in progress — please wait".into());
            return;
        }
        let Some(sel) = self.selected else { return };
        if sel == self.cur {
            return;
        }
        // The core safety rule: never delete a subtree that is still being scanned, or its
        // contents (and on-disk reality) aren't fully known yet.
        if self.tree.nodes[sel].is_dir() && !self.tree.subtree_complete(sel) {
            self.status = Some(
                "cannot delete: directory is still being scanned — wait for it to finish".into(),
            );
            return;
        }
        self.modal = Modal::ConfirmDelete(sel);
    }

    fn perform_delete(&mut self, idx: NodeIdx) {
        let path = self.tree.path_of(idx);
        let is_dir = self.tree.nodes[idx].is_dir();
        // Double-check the gate at the moment of action (scan may have advanced).
        if is_dir && !self.tree.subtree_complete(idx) {
            self.status = Some("cannot delete: subtree not fully scanned".into());
            return;
        }

        // Run the (potentially slow) filesystem removal off the UI thread. The loop polls
        // `poll_delete`, and the UI shows a spinner via `deleting_path` until it completes.
        let (tx, rx) = unbounded();
        let p = path.clone();
        std::thread::spawn(move || {
            let res = if is_dir {
                std::fs::remove_dir_all(&p)
            } else {
                // Covers files, symlinks (removes the link, not the target), and special files.
                std::fs::remove_file(&p)
            };
            let _ = tx.send(res);
        });
        self.status = None;
        self.pending_delete = Some(PendingDelete { idx, path, rx });
    }

    /// Check whether a background deletion has finished and, if so, fold the result into the
    /// tree. Called once per frame by the event loop.
    pub fn poll_delete(&mut self) {
        let Some(pd) = self.pending_delete.as_ref() else {
            return;
        };
        let result = match pd.rx.try_recv() {
            Ok(res) => res,
            Err(crossbeam_channel::TryRecvError::Empty) => return, // still working
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                Err(std::io::Error::other("delete worker exited unexpectedly"))
            }
        };
        let pd = self.pending_delete.take().unwrap();
        // The deletion is over, so a pending force-quit is no longer relevant.
        self.quit_armed = false;
        match result {
            Ok(()) => {
                // If the removed node is in the view the user is looking at, land the highlight
                // on a neighbour; if they browsed elsewhere, leave their selection untouched.
                let kids = self.sorted_children();
                let pos = kids.iter().position(|&k| k == pd.idx);
                self.tree.delete_subtree(pd.idx);
                self.prune_find();
                if let Some(p) = pos {
                    let kids = self.sorted_children();
                    self.selected = (!kids.is_empty()).then(|| kids[p.min(kids.len() - 1)]);
                }
                self.status = Some(format!("deleted {}", pd.path));
            }
            Err(e) => {
                self.status = Some(format!("delete failed: {e}"));
            }
        }
    }
}

/// Launch the platform opener for `path` and wait for it to exit — waiting reaps the child
/// instead of leaving a zombie. Stdio is disconnected so the opener can't scribble on the TUI.
fn open_path(path: &str) -> std::io::Result<()> {
    let status = opener_command(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "opener exited with {status}"
        )))
    }
}

#[cfg(target_os = "macos")]
fn opener_command(path: &str) -> Command {
    let mut cmd = Command::new("open");
    cmd.arg(path);
    cmd
}

#[cfg(target_os = "windows")]
fn opener_command(path: &str) -> Command {
    // `start` treats its first quoted argument as the window title, so pass an empty one.
    let mut cmd = Command::new("cmd");
    cmd.args(["/C", "start", "", path]);
    cmd
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn opener_command(path: &str) -> Command {
    let mut cmd = Command::new("xdg-open");
    cmd.arg(path);
    cmd
}

/// A short type indicator for an entry (mirrors ncdu's column).
pub fn indicator(kind: NodeKind, shared: bool, excluded: bool, read_error: bool) -> char {
    if read_error {
        '!'
    } else if excluded {
        '<'
    } else if shared {
        'H'
    } else {
        match kind {
            NodeKind::Dir => '/',
            NodeKind::Link => '@',
            NodeKind::Other => '*',
            NodeKind::File => ' ',
        }
    }
}

/// Logical input actions, decoded from raw keys in `main`.
#[derive(Clone, Copy)]
pub enum KeyAction {
    Quit,
    Up,
    Down,
    Top,
    Bottom,
    Enter,
    Leave,
    ToggleSort,
    ToggleUsage,
    Help,
    Delete,
    Open,
    Confirm,
    Cancel,
    Search,
    FilterChar(char),
    FilterBackspace,
    FilterConfirm,
    FilterCancel,
    TopFiles,
    Refresh,
    Info,
    /// Jump by this many rows (the visible page height).
    PageDown(usize),
    PageUp(usize),
    Export,
    NextError,
    Shell,
    /// Open the whole-tree search popup.
    Find,
    /// A character typed into the search popup's query.
    FindChar(char),
    FindBackspace,
}

/// First free `rcdu-dump*.json` path in `dir`, so consecutive exports never clobber each
/// other (or an unrelated file that happens to share the name).
fn next_export_path(dir: &Path) -> PathBuf {
    if !dir.join("rcdu-dump.json").exists() {
        return dir.join("rcdu-dump.json");
    }
    (2..)
        .map(|n| dir.join(format!("rcdu-dump-{n}.json")))
        .find(|p| !p.exists())
        .expect("a free path always exists")
}

/// Split Unix seconds into UTC civil date/time (Howard Hinnant's civil-from-days).
fn civil(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (h, mi, s) = (
        (tod / 3600) as u32,
        ((tod % 3600) / 60) as u32,
        (tod % 60) as u32,
    );
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u32, d as u32, h, mi, s)
}

/// Human-readable UTC timestamp for the info panel (mtime), dependency-free.
pub fn fmt_time(secs: i64) -> String {
    let (y, mo, d, h, mi, s) = civil(secs);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02} UTC")
}

/// Case-insensitive substring test used by the `/` filter and the whole-tree search. ASCII
/// letters compare case-insensitively; other bytes must match exactly (a plain byte-substring
/// test, so multi-byte UTF-8 names can't produce false positives). An empty needle matches
/// everything, so a just-opened (still-empty) filter hides nothing.
fn contains_ci(haystack: &str, needle: &str) -> bool {
    find_ci(haystack, needle).is_some()
}

/// Byte offset of the first case-insensitive match of `needle` in `haystack` (same rules as
/// [`contains_ci`]). The match always starts and ends on a char boundary: it begins and ends
/// with the same bytes as the (valid UTF-8) needle.
pub fn find_ci(haystack: &str, needle: &str) -> Option<usize> {
    let (h, n) = (haystack.as_bytes(), needle.as_bytes());
    if n.is_empty() {
        return Some(0);
    }
    h.windows(n.len()).position(|w| w.eq_ignore_ascii_case(n))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Excluded, NodeKind};

    fn app() -> App {
        App::new(Tree::new("/r".into(), 0, 0, 1, 1), true, false, false)
    }

    fn app_with_entries() -> App {
        let mut t = Tree::new_imported("/r".into(), 0, 0, 1, 1);
        for (name, kind, size) in [
            ("alpha", NodeKind::File, 10u64),
            ("Beta", NodeKind::File, 30),
            ("banner", NodeKind::File, 20),
            ("gamma", NodeKind::Dir, 5),
        ] {
            t.import_child(
                t.root,
                name.into(),
                kind,
                size,
                size,
                1,
                1,
                false,
                false,
                Excluded::No,
                false,
            );
        }
        App::new(t, true, false, false)
    }

    fn shown_names(app: &App) -> Vec<&str> {
        app.sorted_children()
            .iter()
            .map(|&k| app.tree.nodes[k].name.as_str())
            .collect()
    }

    #[test]
    fn contains_ci_matches_ascii_case_insensitively() {
        assert!(contains_ci("Hello, World", "WORLD"));
        assert!(contains_ci("node_modules", "ODE_MOD"));
        assert!(contains_ci("anything", ""));
        assert!(!contains_ci("short", "shorter"));
        assert!(
            !contains_ci("résumé", "RESUME"),
            "non-ASCII compares exactly"
        );
        assert!(contains_ci("résumé", "sum"));
    }

    #[test]
    fn filter_narrows_live_and_applies_on_enter() {
        let mut app = app_with_entries();
        // Opening the filter (empty query) hides nothing.
        app.on_key(KeyAction::Search);
        assert_eq!(app.sorted_children().len(), 4);
        app.on_key(KeyAction::FilterChar('b'));
        assert_eq!(shown_names(&app), vec!["Beta", "banner"]);
        assert_eq!(app.tree.nodes[app.selected.unwrap()].name, "Beta");
        assert!(app.searching);

        app.on_key(KeyAction::FilterChar('a'));
        assert_eq!(shown_names(&app), vec!["banner"]);
        assert_eq!(app.tree.nodes[app.selected.unwrap()].name, "banner");

        app.on_key(KeyAction::FilterConfirm);
        assert!(!app.searching);
        assert_eq!(shown_names(&app), vec!["banner"], "filter persists");
    }

    #[test]
    fn filter_cancel_restores_previous_selection() {
        let mut app = app_with_entries();
        app.on_key(KeyAction::Down); // select "banner" (2nd by size: 30, 20, 10, 5)
        let before = app.selected;
        app.on_key(KeyAction::Search);
        app.on_key(KeyAction::FilterChar('g')); // only "gamma" matches
        assert_eq!(shown_names(&app), vec!["gamma"]);
        assert_ne!(app.selected, before);

        app.on_key(KeyAction::FilterCancel);
        assert!(app.filter.is_none());
        assert_eq!(app.selected, before, "selection restored");
        assert_eq!(shown_names(&app).len(), 4);
    }

    #[test]
    fn empty_query_confirms_to_no_filter() {
        let mut app = app_with_entries();
        app.on_key(KeyAction::Search);
        app.on_key(KeyAction::FilterChar('z'));
        assert!(app.sorted_children().is_empty());
        app.on_key(KeyAction::FilterBackspace);
        app.on_key(KeyAction::FilterConfirm);
        assert!(app.filter.is_none(), "empty query clears the filter");
        assert_eq!(app.sorted_children().len(), 4);
    }

    fn app_with_tree_for_top_files() -> App {
        let mut t = Tree::new_imported("/r".into(), 0, 0, 1, 1);
        t.import_child(
            t.root,
            "big.bin".into(),
            NodeKind::File,
            900,
            900,
            1,
            1,
            false,
            false,
            Excluded::No,
            false,
        );
        let sub = t.import_child(
            t.root,
            "sub".into(),
            NodeKind::Dir,
            500,
            500,
            1,
            2,
            false,
            false,
            Excluded::No,
            false,
        );
        t.import_child(
            sub,
            "mid.bin".into(),
            NodeKind::File,
            500,
            500,
            1,
            3,
            false,
            false,
            Excluded::No,
            false,
        );
        t.import_child(
            t.root,
            "ln".into(),
            NodeKind::Link,
            100,
            100,
            1,
            4,
            false,
            false,
            Excluded::No,
            false,
        );
        App::new(t, true, false, false)
    }

    #[test]
    fn top_files_lists_files_largest_first_with_relative_paths() {
        let app = app_with_tree_for_top_files();
        let top = app.top_files(app.tree.root, 10);
        assert_eq!(
            top.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(),
            vec!["big.bin", "sub/mid.bin"],
            "files only (no dirs, no symlinks), largest first, relative paths"
        );
        assert_eq!(top[0].size, 900);
        assert_eq!(app.top_files(app.tree.root, 1).len(), 1, "limit applies");
    }

    #[test]
    fn top_files_popup_navigation_and_jump() {
        let mut app = app_with_tree_for_top_files();
        app.on_key(KeyAction::TopFiles);
        let items = match &app.modal {
            Modal::TopFiles(tf) => tf.items.len(),
            _ => panic!("top-files popup did not open"),
        };
        assert_eq!(items, 2);
        app.on_key(KeyAction::Down); // select the second row (sub/mid.bin)
        app.on_key(KeyAction::Enter);
        assert!(matches!(app.modal, Modal::None), "jump closes the popup");
        assert_eq!(
            app.tree.nodes[app.cur].name, "sub",
            "landed in the parent dir"
        );
        assert_eq!(
            app.tree.nodes[app.selected.unwrap()].name,
            "mid.bin",
            "file is selected"
        );
    }

    /// A finished open lands in the status line and drains its slot, so the UI thread never
    /// waited on the opener.
    #[test]
    fn finished_open_reports_result_and_drains() {
        let mut a = app();
        let (tx, rx) = unbounded();
        tx.send(Err(std::io::Error::other("no handler"))).unwrap();
        drop(tx);
        a.pending_open.push(PendingOpen {
            path: "/r/a".into(),
            rx,
        });

        a.poll_open();
        assert_eq!(a.status.as_deref(), Some("open failed: no handler"));
        assert!(a.pending_open.is_empty());
    }

    /// An opener that hasn't exited yet keeps its slot (and reports nothing), then resolves on
    /// a later frame.
    #[test]
    fn in_flight_open_waits_and_then_resolves() {
        let mut a = app();
        let (tx, rx) = unbounded();
        a.pending_open.push(PendingOpen {
            path: "/r/a".into(),
            rx,
        });

        a.poll_open();
        assert_eq!(a.pending_open.len(), 1);
        assert_eq!(a.status, None);

        tx.send(Ok(())).unwrap();
        drop(tx);
        a.poll_open();
        assert_eq!(a.status.as_deref(), Some("opened /r/a"));
        assert!(a.pending_open.is_empty());
    }

    /// PageUp/PageDown jump by the given page size and clamp at the ends.
    #[test]
    fn page_keys_jump_and_clamp() {
        let mut app = app_with_entries();
        // Sorted by size: Beta(30), banner(20), alpha(10), gamma(5).
        app.on_key(KeyAction::PageDown(2));
        assert_eq!(app.tree.nodes[app.selected.unwrap()].name, "alpha");
        app.on_key(KeyAction::PageDown(99));
        assert_eq!(app.tree.nodes[app.selected.unwrap()].name, "gamma");
        app.on_key(KeyAction::PageUp(2));
        assert_eq!(app.tree.nodes[app.selected.unwrap()].name, "banner");
        app.on_key(KeyAction::PageUp(99));
        assert_eq!(app.tree.nodes[app.selected.unwrap()].name, "Beta");
    }

    /// The footer's throughput figure: bytes/sec while a scan runs, none otherwise.
    #[test]
    fn scan_rate_reports_bytes_per_second_while_scanning() {
        let mut app = app_with_entries();
        assert_eq!(app.scan_rate(), None, "no scan attached yet");

        app.scanning = true;
        app.scan_started = Some(Instant::now());
        app.bytes_seen = 4_000_000;
        // Elapsed is sub-second, so the rate is the bytes seen in the first second.
        assert_eq!(app.scan_rate(), Some(4_000_000));

        app.scanning = false;
        assert_eq!(app.scan_rate(), None);
    }

    /// The info popup's timestamps (dependency-free UTC formatting, leap days included).
    #[test]
    fn fmt_time_formats_utc() {
        assert_eq!(fmt_time(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(fmt_time(86_400), "1970-01-02 00:00:00 UTC");
        assert_eq!(fmt_time(951_782_400), "2000-02-29 00:00:00 UTC");
        assert_eq!(fmt_time(1_000_000_000), "2001-09-09 01:46:40 UTC");
        assert_eq!(fmt_time(1_700_000_000), "2023-11-14 22:13:20 UTC");
    }

    /// With no matching on-disk file (tree paths under "/r" don't exist here), the popup shows
    /// tree data and degrades gracefully instead of failing.
    #[test]
    fn info_rows_show_tree_data_and_note_missing_disk_entry() {
        let app = app_with_entries();
        let beta = app.tree.nodes[app.tree.root].children[1];
        let rows = app.info_pairs(beta);
        assert!(rows.contains(&("path".to_string(), "/r/Beta".to_string())));
        assert!(rows.contains(&("type".to_string(), "file".to_string())));
        let on_disk = rows
            .iter()
            .find(|(k, _)| k == "on disk")
            .expect("on-disk row present");
        assert!(on_disk.1.starts_with("unavailable"), "got {on_disk:?}");
    }

    /// For a live file, the on-disk section is statted: octal permissions, owner uid:gid, and
    /// a formatted modification time.
    #[test]
    fn info_rows_stat_live_files() {
        use std::fs;
        let base = std::env::temp_dir().join(format!("rcdu_info_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        fs::write(base.join("f"), b"0123456789").unwrap();
        let meta = fs::symlink_metadata(&base).unwrap();
        let mut t = Tree::new(
            base.to_string_lossy().into(),
            meta.len(),
            meta.blocks() * 512,
            meta.dev(),
            meta.ino(),
        );
        let f = t.import_child(
            t.root,
            "f".into(),
            NodeKind::File,
            10,
            4096,
            meta.dev(),
            1,
            false,
            false,
            Excluded::No,
            false,
        );
        let app = App::new(t, true, false, false);
        let rows = app.info_pairs(f);

        let perms = rows.iter().find(|(k, _)| k == "permissions").unwrap();
        assert!(i64::from_str_radix(&perms.1, 8).is_ok(), "got {perms:?}");
        let owner = rows.iter().find(|(k, _)| k == "owner").unwrap();
        assert!(owner.1.contains(':'), "got {owner:?}");
        let modified = rows.iter().find(|(k, _)| k == "modified").unwrap();
        assert_eq!(
            modified.1.len(),
            23,
            "YYYY-MM-DD HH:MM:SS UTC: {modified:?}"
        );
        assert!(
            rows.iter().all(|(k, _)| k != "entries" && k != "on disk"),
            "no dir-only or error rows for a live file"
        );

        fs::remove_dir_all(&base).unwrap();
    }

    /// The info popup is read-only: any key closes it, and q must not quit through it.
    #[test]
    fn info_modal_closes_on_any_key_without_quitting() {
        let mut app = app_with_entries();
        app.on_key(KeyAction::Info);
        assert!(matches!(app.modal, Modal::Info(_)));
        app.on_key(KeyAction::Quit);
        assert!(matches!(app.modal, Modal::None));
        assert!(!app.quit, "closing info must not quit");
    }

    /// An exported dump is readable again by the importer, with the same totals and children.
    #[test]
    fn export_writes_an_importable_dump() {
        let app = app_with_entries();
        let dir = std::env::temp_dir().join(format!("rcdu_export_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dump.json");
        app.export_tree_to(&path).unwrap();
        let re = crate::dump::import(std::io::BufReader::new(std::fs::File::open(&path).unwrap()))
            .unwrap();
        assert_eq!(
            re.nodes[re.root].apparent, app.tree.nodes[app.tree.root].apparent,
            "totals survive the round-trip"
        );
        assert_eq!(re.nodes[re.root].children.len(), 4);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Consecutive exports pick the first free `rcdu-dump*.json` name instead of clobbering.
    #[test]
    fn export_paths_avoid_clobbering() {
        let dir = std::env::temp_dir().join(format!("rcdu_export_name_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(
            next_export_path(&dir).file_name().unwrap(),
            "rcdu-dump.json"
        );
        std::fs::write(dir.join("rcdu-dump.json"), b"x").unwrap();
        assert_eq!(
            next_export_path(&dir).file_name().unwrap(),
            "rcdu-dump-2.json"
        );
        std::fs::write(dir.join("rcdu-dump-2.json"), b"x").unwrap();
        assert_eq!(
            next_export_path(&dir).file_name().unwrap(),
            "rcdu-dump-3.json"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// An app whose entries include read errors, in display order (by size):
    /// big(500), broken(400, error), brokendir(300, error, has a child), small(100).
    fn app_with_errors() -> App {
        let mut t = Tree::new_imported("/r".into(), 0, 0, 1, 1);
        for (name, kind, size, err) in [
            ("big", NodeKind::File, 500u64, false),
            ("broken", NodeKind::File, 400, true),
            ("brokendir", NodeKind::Dir, 300, true),
            ("small", NodeKind::File, 100, false),
        ] {
            t.import_child(
                t.root,
                name.into(),
                kind,
                size,
                size,
                1,
                1,
                false,
                false,
                Excluded::No,
                err,
            );
        }
        App::new(t, true, false, false)
    }

    /// The jump lands on each error entry in display order, moves the view to its parent,
    /// and wraps around after the last one.
    #[test]
    fn next_error_jumps_in_display_order_and_wraps() {
        let mut app = app_with_errors();
        // Initial selection is the first row ("big"); the first error after it is "broken".
        app.on_key(KeyAction::NextError);
        assert_eq!(app.tree.nodes[app.selected.unwrap()].name, "broken");
        assert_eq!(app.tree.nodes[app.cur].name, "/r");
        assert_eq!(app.status.as_deref(), Some("read error: /r/broken"));

        app.on_key(KeyAction::NextError);
        assert_eq!(app.tree.nodes[app.selected.unwrap()].name, "brokendir");

        // Wrapped: both errors seen, back to the first.
        app.on_key(KeyAction::NextError);
        assert_eq!(app.tree.nodes[app.selected.unwrap()].name, "broken");
    }

    /// With no errors anywhere, nothing moves and the footer says so.
    #[test]
    fn next_error_without_errors_reports_none() {
        let mut app = app_with_entries();
        app.on_key(KeyAction::Down); // settle the selection (first key always selects)
        let before = app.selected;
        app.on_key(KeyAction::NextError);
        assert_eq!(app.selected, before);
        assert_eq!(app.status.as_deref(), Some("no read errors"));
    }

    /// `b` targets the selected directory, or — for a file — the directory containing it.
    #[test]
    fn shell_request_targets_selected_dir_or_parent_of_file() {
        let mut app = app_with_entries();
        // No selection yet: falls back to the current directory.
        app.on_key(KeyAction::Shell);
        assert_eq!(app.take_shell_request().as_deref(), Some("/r"));

        // "alpha" (two downs from Beta) is a file: the shell goes to its parent, not the file.
        app.on_key(KeyAction::Down);
        app.on_key(KeyAction::Down);
        app.on_key(KeyAction::Shell);
        assert_eq!(app.take_shell_request().as_deref(), Some("/r"));

        // "gamma" is a directory: the shell opens there.
        app.on_key(KeyAction::Down);
        app.on_key(KeyAction::Shell);
        assert_eq!(app.take_shell_request().as_deref(), Some("/r/gamma"));
        assert!(app.take_shell_request().is_none(), "request is consumed");
    }

    // ---- Whole-tree search (`f`) ------------------------------------------------------

    fn file_under(t: &mut Tree, parent: NodeIdx, name: &str, size: u64) -> NodeIdx {
        t.import_child(
            parent,
            name.into(),
            NodeKind::File,
            size,
            size,
            1,
            1,
            false,
            false,
            Excluded::No,
            false,
        )
    }

    /// /r: docs/ { readme.md (50), sub/ { README.txt (200) } }, readme (10), other.bin (1000).
    fn app_for_find() -> App {
        let mut t = Tree::new_imported("/r".into(), 0, 0, 1, 1);
        let docs = t.import_child(
            t.root,
            "docs".into(),
            NodeKind::Dir,
            0,
            0,
            1,
            2,
            false,
            false,
            Excluded::No,
            false,
        );
        file_under(&mut t, docs, "readme.md", 50);
        let sub = t.import_child(
            docs,
            "sub".into(),
            NodeKind::Dir,
            0,
            0,
            1,
            3,
            false,
            false,
            Excluded::No,
            false,
        );
        file_under(&mut t, sub, "README.txt", 200);
        file_under(&mut t, 0, "readme", 10);
        file_under(&mut t, 0, "other.bin", 1000);
        App::new(t, true, false, false)
    }

    fn find_state(app: &App) -> &Find {
        match &app.modal {
            Modal::Find(f) => f,
            _ => panic!("search popup is not open"),
        }
    }

    fn type_query(app: &mut App, q: &str) {
        for c in q.chars() {
            app.on_key(KeyAction::FindChar(c));
        }
    }

    fn hit_names(app: &App) -> Vec<String> {
        find_state(app)
            .hits
            .iter()
            .map(|&i| app.tree.nodes[i].name.to_string())
            .collect()
    }

    /// Open the popup, type `q`, and run the debounced pass to completion.
    fn search(app: &mut App, q: &str) -> Instant {
        let t0 = Instant::now();
        app.on_key(KeyAction::Find);
        type_query(app, q);
        app.find_tick(t0, usize::MAX);
        app.find_tick(t0 + FIND_DEBOUNCE, usize::MAX);
        t0 + FIND_DEBOUNCE
    }

    fn idx_named(app: &App, name: &str) -> NodeIdx {
        app.tree.nodes.iter().position(|n| n.name == name).unwrap()
    }

    /// Matches come from every level, case-insensitively, largest first; the root (whose
    /// name is the absolute scan path) is never a hit.
    #[test]
    fn find_matches_across_whole_tree() {
        let mut app = app_for_find();
        search(&mut app, "readme");
        assert_eq!(hit_names(&app), vec!["README.txt", "readme.md", "readme"]);
        assert_eq!(find_state(&app).total, 3);
        assert!(!app.find_busy());
        assert_eq!(
            find_state(&app).selected,
            Some(idx_named(&app, "README.txt")),
            "first hit selected"
        );

        let mut app = app_for_find();
        search(&mut app, "/r");
        assert!(hit_names(&app).is_empty(), "root excluded");
    }

    /// The pass runs only once the query has been left alone for the debounce interval, and
    /// every keystroke restarts the wait.
    #[test]
    fn find_debounces_100ms() {
        let mut app = app_for_find();
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        app.on_key(KeyAction::Find);
        app.on_key(KeyAction::FindChar('r'));
        app.find_tick(t0, usize::MAX);
        assert_eq!(app.find_due(), Some(t0 + FIND_DEBOUNCE));
        app.find_tick(t0 + ms(99), usize::MAX);
        assert!(find_state(&app).active.is_none(), "too early");

        // Another key at 99 ms pushes the deadline out again.
        app.on_key(KeyAction::FindChar('e'));
        app.find_tick(t0 + ms(99), usize::MAX);
        app.find_tick(t0 + ms(100), usize::MAX);
        assert!(
            find_state(&app).active.is_none(),
            "re-armed by the second key"
        );
        app.find_tick(t0 + ms(199), usize::MAX);
        assert_eq!(find_state(&app).active.as_deref(), Some("re"));
        assert_eq!(app.find_due(), None);
        assert_eq!(find_state(&app).total, 3);

        // Erasing the query clears the results once the debounce fires.
        app.on_key(KeyAction::FindBackspace);
        app.on_key(KeyAction::FindBackspace);
        app.find_tick(t0 + ms(200), usize::MAX);
        app.find_tick(t0 + ms(300), usize::MAX);
        assert!(find_state(&app).active.is_none());
        assert!(hit_names(&app).is_empty());
    }

    /// With a small budget the pass is spread over several ticks, reporting busy meanwhile.
    #[test]
    fn find_slices_with_budget() {
        let mut app = app_for_find();
        let t0 = Instant::now();
        app.on_key(KeyAction::Find);
        type_query(&mut app, "readme");
        app.find_tick(t0, 0);
        let mut now = t0 + FIND_DEBOUNCE;
        let mut ticks = 0;
        loop {
            app.find_tick(now, 2);
            ticks += 1;
            if !app.find_busy() {
                break;
            }
            assert!(find_state(&app).cursor < app.tree.nodes.len());
            now += Duration::from_millis(1);
        }
        assert_eq!(ticks, app.tree.nodes.len().div_ceil(2));
        assert_eq!(hit_names(&app), vec!["README.txt", "readme.md", "readme"]);
    }

    /// A broad query keeps at most FIND_CAP hits but still counts all matches.
    #[test]
    fn find_caps_hits_but_counts_all() {
        let mut t = Tree::new_imported("/r".into(), 0, 0, 1, 1);
        for i in 0..FIND_CAP + 5 {
            file_under(&mut t, 0, &format!("hit{i}"), i as u64);
        }
        let mut app = App::new(t, true, false, false);
        search(&mut app, "hit");
        let f = find_state(&app);
        assert_eq!(f.hits.len(), FIND_CAP);
        assert_eq!(f.total, FIND_CAP + 5);
        assert!(f.capped());
    }

    /// Entries a running scan adds after the pass are picked up by later ticks.
    #[test]
    fn find_picks_up_live_inserts() {
        use crate::scan::NewNode;
        let node = |name: &str, kind, dir_id| NewNode {
            name: name.into(),
            kind,
            apparent: 10,
            disk: 10,
            dev: 1,
            ino: 1,
            hlink: false,
            shared: false,
            excluded: Excluded::No,
            read_error: false,
            dir_id,
        };
        let mut app = App::new(Tree::new("/r".into(), 0, 0, 1, 1), true, false, true);
        app.tree.apply(Batch {
            parent_id: 0,
            nodes: vec![
                node("log", NodeKind::Dir, Some(1)),
                node("a.log", NodeKind::File, None),
            ],
            error: None,
        });
        let now = search(&mut app, ".log");
        assert_eq!(hit_names(&app), vec!["a.log"]);

        app.tree.apply(Batch {
            parent_id: 1,
            nodes: vec![
                node("b.log", NodeKind::File, None),
                node("x", NodeKind::File, None),
            ],
            error: None,
        });
        assert!(app.find_busy(), "new nodes to look at");
        app.find_tick(now + Duration::from_secs(1), usize::MAX);
        assert_eq!(hit_names(&app), vec!["a.log", "b.log"]);
        assert!(!app.find_busy());
    }

    /// Deleting a subtree drops its hits, and a new pass doesn't find the tombstones.
    #[test]
    fn find_drops_deleted_entries() {
        let mut app = app_for_find();
        search(&mut app, "readme");
        app.tree.delete_subtree(idx_named(&app, "docs"));
        app.prune_find();
        assert_eq!(hit_names(&app), vec!["readme"]);
        assert_eq!(find_state(&app).total, 1);
        assert_eq!(find_state(&app).selected, Some(idx_named(&app, "readme")));

        // Retype: the fresh pass walks over the tombstones but skips them.
        app.on_key(KeyAction::FindBackspace);
        app.on_key(KeyAction::FindChar('e'));
        let t = Instant::now();
        app.find_tick(t, usize::MAX);
        app.find_tick(t + FIND_DEBOUNCE, usize::MAX);
        assert_eq!(hit_names(&app), vec!["readme"]);
    }

    /// A rescan tears the old subtree down: its hits go; the rescanned directory stays.
    #[test]
    fn find_drops_rescanned_entries() {
        let mut app = app_for_find();
        search(&mut app, "d");
        // docs/ aggregates its children (250), so it sorts first.
        assert_eq!(
            hit_names(&app),
            vec!["docs", "README.txt", "readme.md", "readme"]
        );
        let docs = idx_named(&app, "docs");
        app.tree.begin_refresh(docs, SCAN_ID_STRIDE);
        app.prune_find();
        assert_eq!(hit_names(&app), vec!["docs", "readme"]);
        assert_eq!(find_state(&app).total, 2);
    }

    /// Past the cap the count can't be fixed up from the kept hits: the pass starts over.
    #[test]
    fn find_prune_restarts_a_capped_pass() {
        let mut t = Tree::new_imported("/r".into(), 0, 0, 1, 1);
        let dir = t.import_child(
            t.root,
            "dir".into(),
            NodeKind::Dir,
            0,
            0,
            1,
            2,
            false,
            false,
            Excluded::No,
            false,
        );
        for i in 0..FIND_CAP + 5 {
            file_under(&mut t, dir, &format!("hit{i}"), 1);
        }
        file_under(&mut t, 0, "hit-root", 1);
        let mut app = App::new(t, true, false, false);
        let now = search(&mut app, "hit");
        assert!(find_state(&app).capped());

        app.tree.delete_subtree(dir);
        app.prune_find();
        assert!(app.find_busy(), "restarted");
        app.find_tick(now, usize::MAX);
        assert_eq!(hit_names(&app), vec!["hit-root"]);
        assert_eq!(find_state(&app).total, 1);
    }

    /// Enter moves the view to the hit's parent with the hit selected and closes the popup.
    #[test]
    fn find_enter_jumps_to_parent_with_match_selected() {
        let mut app = app_for_find();
        search(&mut app, "readme");
        app.on_key(KeyAction::Down); // readme.md, in docs/
        app.on_key(KeyAction::Enter);
        assert!(matches!(app.modal, Modal::None));
        assert_eq!(app.cur, idx_named(&app, "docs"));
        assert_eq!(app.selected, Some(idx_named(&app, "readme.md")));
    }

    /// Esc closes the popup and leaves the view untouched; Enter without hits does nothing.
    #[test]
    fn find_esc_leaves_view_unchanged() {
        let mut app = app_for_find();
        app.on_key(KeyAction::Down);
        let (cur, sel) = (app.cur, app.selected);
        search(&mut app, "zzz");
        app.on_key(KeyAction::Enter);
        assert!(matches!(app.modal, Modal::Find(_)), "nothing to jump to");
        app.on_key(KeyAction::Cancel);
        assert!(matches!(app.modal, Modal::None));
        assert_eq!((app.cur, app.selected), (cur, sel));
        assert!(!app.quit);
    }

    /// Ctrl-C (Quit) still quits while the popup is open; letters like `q` only type.
    #[test]
    fn find_quit_key_quits_but_q_types() {
        let mut app = app_for_find();
        app.on_key(KeyAction::Find);
        app.on_key(KeyAction::FindChar('q'));
        assert!(!app.quit);
        assert_eq!(find_state(&app).query, "q");
        app.on_key(KeyAction::Quit);
        assert!(app.quit);
    }

    /// The highlight follows the selected entry when a re-sort moves it, and navigation
    /// clamps at both ends.
    #[test]
    fn find_selection_tracks_identity_across_resort() {
        let mut app = app_for_find();
        let now = search(&mut app, "readme");
        app.on_key(KeyAction::Down);
        let md = idx_named(&app, "readme.md");
        assert_eq!(find_state(&app).selected, Some(md));

        // A bigger match appears (appended, as a scan would) and sorts to the top.
        let root = app.tree.root;
        file_under(&mut app.tree, root, "readme.huge", 9999);
        app.find_tick(now + Duration::from_secs(1), usize::MAX);
        assert_eq!(hit_names(&app)[0], "readme.huge");
        assert_eq!(
            find_state(&app).selected,
            Some(md),
            "same entry still selected"
        );
        assert_eq!(find_state(&app).selected_pos(), Some(2));

        app.on_key(KeyAction::PageDown(99));
        assert_eq!(find_state(&app).selected_pos(), Some(3));
        app.on_key(KeyAction::PageUp(99));
        assert_eq!(find_state(&app).selected_pos(), Some(0));
    }
}
