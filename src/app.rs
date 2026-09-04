//! Application state and input handling.
//!
//! Selection tracks a node's *identity*, not its row position, so the highlight stays glued to
//! the same entry even as live size updates reshuffle the sort order during a scan.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use crossbeam_channel::{unbounded, Receiver};

use crate::model::{NodeIdx, NodeKind, Tree};
use crate::scan::{Batch, ScanControl};

#[derive(Clone, Copy, PartialEq)]
pub enum SortKey {
    Size,
    Name,
}

/// A transient modal overlay.
pub enum Modal {
    None,
    Help,
    /// Confirm deletion of the given node.
    ConfirmDelete(NodeIdx),
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
    /// Handle to steer the live scan (prioritize the focused directory). None when browsing a
    /// loaded dump (`-f`), where there is no scan.
    scan_control: Option<ScanControl>,
    /// Spinner animation frame.
    pub tick: usize,
}

impl App {
    pub fn new(tree: Tree, disk_usage: bool, read_only: bool, scanning: bool) -> Self {
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
            scan_control: None,
            tick: 0,
            tree,
        }
    }

    /// Attach the live-scan steering handle (interactive scan mode only).
    pub fn attach_scan(&mut self, control: ScanControl) {
        self.scan_control = Some(control);
        self.refocus();
    }

    /// Tell the scanner to prioritize the directory currently being viewed.
    fn refocus(&mut self) {
        if !self.scanning {
            return;
        }
        if let Some(ctrl) = &self.scan_control {
            ctrl.set_focus(Some(PathBuf::from(self.tree.path_of(self.cur))));
        }
    }

    pub fn is_deleting(&self) -> bool {
        self.pending_delete.is_some()
    }

    pub fn apply(&mut self, batch: Batch) {
        self.tree.apply(batch);
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
        let mut kids = self.tree.nodes[self.cur].children.clone();
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
        if let Some(q) = &self.filter {
            kids.retain(|&k| contains_ci(&self.tree.nodes[k].name, q));
        }
        kids
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

    /// The node currently being deleted (its subtree is locked), if any.
    pub fn deleting_idx(&self) -> Option<NodeIdx> {
        self.pending_delete.as_ref().map(|p| p.idx)
    }

    pub fn on_key(&mut self, action: KeyAction) {
        // Note: a background deletion does NOT block input — the user can keep browsing. Only
        // the subtree being deleted is locked (see `descend` and `request_delete`).

        // Modal overlays capture input first.
        match self.modal {
            Modal::Help => {
                self.modal = Modal::None;
                return;
            }
            Modal::ConfirmDelete(idx) => {
                match action {
                    KeyAction::Confirm => {
                        self.modal = Modal::None;
                        self.perform_delete(idx);
                    }
                    KeyAction::Quit | KeyAction::Leave | KeyAction::Cancel => {
                        self.modal = Modal::None;
                        self.status = Some("delete cancelled".into());
                    }
                    _ => {}
                }
                return;
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
            KeyAction::Confirm | KeyAction::Cancel => {}
            // Handled by the search-mode block above; unreachable here.
            KeyAction::FilterChar(_)
            | KeyAction::FilterBackspace
            | KeyAction::FilterConfirm
            | KeyAction::FilterCancel => {}
        }
    }

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
}

/// Case-insensitive substring test used by the `/` filter. ASCII letters compare
/// case-insensitively; other bytes must match exactly (a plain byte-substring test, so
/// multi-byte UTF-8 names can't produce false positives). An empty needle matches everything,
/// so a just-opened (still-empty) filter hides nothing.
fn contains_ci(haystack: &str, needle: &str) -> bool {
    let (h, n) = (haystack.as_bytes(), needle.as_bytes());
    if n.is_empty() {
        return true;
    }
    h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
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
}
