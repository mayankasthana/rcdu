//! Parallel, streaming filesystem scanner.
//!
//! Design goals:
//!  * **Streaming** — every directory we finish reading is published immediately as a
//!    `Batch`, so the UI can render and aggregate partial results long before the whole
//!    tree is walked.
//!  * **SSD-friendly** — directory reads and `lstat` calls are the bottleneck, and they are
//!    latency-bound rather than bandwidth-bound. We run many worker threads (defaulting to
//!    `cores * 4`) so the kernel always has a deep queue of outstanding I/O. On flash this
//!    overlaps thousands of tiny stat latencies; on a spinning disk it would thrash, which is
//!    exactly the trade-off ncdu can't make because it is single-threaded.
//!
//! Work is pulled from a shared **two-tier frontier**: a `hot` queue of jobs whose path lies
//! under the directory the user is currently viewing, and a `cold` queue for everything else.
//! Workers always drain `hot` first, so navigating into a directory makes its subtree finish
//! scanning ahead of unrelated branches (see [`ScanControl::set_focus`]).
//!
//! Shutdown is tracked by a `pending` count (queued + in-flight jobs): when it reaches zero the
//! frontier is marked `done` and all idle workers are woken to exit, which drops every event
//! `Sender` and disconnects the channel — the UI's signal that the scan is complete.

use std::collections::{HashSet, VecDeque};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use compact_str::CompactString;
use crossbeam_channel::{unbounded, Receiver, Sender};

use crate::glob;
use crate::model::{Excluded, NodeKind};

/// A single freshly-discovered filesystem entry, sent to the UI to be grafted into the tree.
pub struct NewNode {
    pub name: CompactString,
    pub kind: NodeKind,
    /// Logical file size in bytes (`st_size`).
    pub apparent: u64,
    /// Actual blocks consumed on disk in bytes (`st_blocks * 512`).
    pub disk: u64,
    pub dev: u64,
    pub ino: u64,
    pub hlink: bool,
    pub shared: bool,
    pub excluded: Excluded,
    pub read_error: bool,
    /// For directories we will recurse into: the id future batches use as `parent_id`.
    pub dir_id: Option<u64>,
}

/// One message per directory we finished reading.
pub struct Batch {
    pub parent_id: u64,
    pub nodes: Vec<NewNode>,
    /// Set if the directory could not be read (e.g. permission denied).
    pub error: Option<String>,
}

/// A unit of work: read directory `path`, whose tree id is `id`.
struct DirJob {
    id: u64,
    path: PathBuf,
}

/// The shared pool of pending directory jobs, split by priority against the focused path.
struct Frontier {
    /// The directory the user is currently viewing; jobs under it are prioritized.
    focus: Option<PathBuf>,
    /// Jobs whose path is under `focus`.
    hot: VecDeque<DirJob>,
    /// All other pending jobs.
    cold: VecDeque<DirJob>,
    /// Queued + in-flight jobs. The scan is finished when this hits zero.
    pending: usize,
    done: bool,
}

impl Frontier {
    fn is_hot(focus: &Option<PathBuf>, path: &Path) -> bool {
        focus.as_ref().is_some_and(|f| path.starts_with(f))
    }

    fn push(&mut self, job: DirJob) {
        if Self::is_hot(&self.focus, &job.path) {
            self.hot.push_back(job);
        } else {
            self.cold.push_back(job);
        }
    }

    fn pop(&mut self) -> Option<DirJob> {
        self.hot.pop_front().or_else(|| self.cold.pop_front())
    }
}

/// Handle the UI keeps to steer the scan. Crucially it does NOT hold the event `Sender`, so the
/// channel still disconnects when the workers finish even while the UI keeps this handle.
pub struct ScanControl {
    frontier: Arc<(Mutex<Frontier>, Condvar)>,
}

impl ScanControl {
    /// Prioritize the subtree rooted at `path` (the directory the user navigated into). Pending
    /// jobs are re-partitioned so anything under `path` is scanned before unrelated branches.
    pub fn set_focus(&self, path: Option<PathBuf>) {
        let (lock, _cv) = &*self.frontier;
        let mut f = lock.lock().unwrap();
        f.focus = path;
        // Re-bucket every queued job against the new focus. Navigation is rare, so the O(n)
        // pass over the frontier is negligible. Workers that are mid-directory will pick from
        // the re-prioritized queues on their next pop; no need to wake anyone.
        let mut all = std::mem::take(&mut f.hot);
        all.extend(std::mem::take(&mut f.cold));
        for job in all {
            f.push(job);
        }
    }
}

/// What [`start`] returns: the batch stream plus the steering handle.
pub struct Scan {
    pub events: Receiver<Batch>,
    pub control: ScanControl,
}

/// Scan configuration.
#[derive(Clone)]
pub struct Opts {
    pub threads: usize,
    /// Don't cross filesystem boundaries (ncdu `-x`).
    pub one_file_system: bool,
    /// Glob patterns matched against entry basenames (ncdu `--exclude`).
    pub excludes: Vec<String>,
    /// Device id of the root, used for `one_file_system`.
    pub root_dev: u64,
    /// `--older-than`: entries modified before this Unix time (seconds) are excluded.
    pub older_than: Option<i64>,
    /// `--newer-than`: entries modified after this Unix time (seconds) are excluded.
    pub newer_than: Option<i64>,
}

/// State shared by workers only (NOT the UI), so it holds the event `Sender`.
struct Shared {
    ev_tx: Sender<Batch>,
    next_id: AtomicU64,
    one_file_system: bool,
    root_dev: u64,
    excludes: Vec<String>,
    /// `--older-than` cutoff in Unix seconds; entries modified before it are excluded.
    older_than: Option<i64>,
    /// `--newer-than` cutoff in Unix seconds; entries modified after it are excluded.
    newer_than: Option<i64>,
    /// (dev, ino) of hard-linked inodes already counted, so duplicates can be flagged.
    seen_hardlinks: Mutex<HashSet<(u64, u64)>>,
}

type FrontierArc = Arc<(Mutex<Frontier>, Condvar)>;

/// Default worker count. SSDs love a deep I/O queue, so we oversubscribe cores heavily;
/// the work is almost entirely waiting on `readdir`/`lstat`, not CPU.
pub fn default_threads() -> usize {
    let cores = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    (cores * 4).clamp(4, 64)
}

/// Id-space width of one scan generation. A scan started with `id_base` uses it for its root
/// directory and allocates further ids above it. Subtree rescans run concurrently with the
/// original scan, so each generation gets a disjoint id range (see [`start_at`]).
pub const SCAN_ID_STRIDE: u64 = 1 << 40;

/// Start scanning `root` (already assigned tree id 0).
/// The event channel disconnects when the scan is done.
pub fn start(root: PathBuf, opts: Opts) -> Scan {
    start_at(root, opts, 0)
}

/// Start scanning `root`, treating it as tree id `id_base`: a whole-tree scan passes 0, while
/// a subtree rescan passes its generation's base (a multiple of [`SCAN_ID_STRIDE`]) so its ids
/// never collide with another scan streaming into the same tree.
pub fn start_at(root: PathBuf, opts: Opts, id_base: u64) -> Scan {
    let (ev_tx, ev_rx) = unbounded::<Batch>();

    let frontier: FrontierArc = Arc::new((
        Mutex::new(Frontier {
            focus: None,
            hot: VecDeque::new(),
            cold: VecDeque::from([DirJob {
                id: id_base,
                path: root,
            }]),
            pending: 1, // the root job
            done: false,
        }),
        Condvar::new(),
    ));

    let shared = Arc::new(Shared {
        ev_tx,
        next_id: AtomicU64::new(id_base + 1),
        one_file_system: opts.one_file_system,
        root_dev: opts.root_dev,
        excludes: opts.excludes,
        older_than: opts.older_than,
        newer_than: opts.newer_than,
        seen_hardlinks: Mutex::new(HashSet::new()),
    });

    for _ in 0..opts.threads {
        let frontier = Arc::clone(&frontier);
        let shared = Arc::clone(&shared);
        thread::spawn(move || worker(frontier, shared));
    }

    // Drop our worker-state reference so the event channel is kept alive only by the workers;
    // it disconnects once the last worker exits. The UI keeps `control` (frontier only).
    drop(shared);

    Scan {
        events: ev_rx,
        control: ScanControl { frontier },
    }
}

fn worker(frontier: FrontierArc, shared: Arc<Shared>) {
    let (lock, cv) = &*frontier;
    loop {
        // Take the next job, preferring the focused subtree; block until one is available or
        // the whole scan is finished.
        let job = {
            let mut f = lock.lock().unwrap();
            loop {
                if let Some(job) = f.pop() {
                    break job;
                }
                if f.done {
                    return;
                }
                f = cv.wait(f).unwrap();
            }
        };

        process(&job, &frontier, &shared);

        // This job is complete. If it was the last one, finish the scan and wake everyone.
        let mut f = lock.lock().unwrap();
        f.pending -= 1;
        if f.pending == 0 {
            f.done = true;
            cv.notify_all();
        }
    }
}

fn process(job: &DirJob, frontier: &FrontierArc, shared: &Shared) {
    let read = match std::fs::read_dir(&job.path) {
        Ok(r) => r,
        Err(e) => {
            let _ = shared.ev_tx.send(Batch {
                parent_id: job.id,
                nodes: Vec::new(),
                error: Some(e.to_string()),
            });
            return;
        }
    };

    let mut nodes = Vec::new();
    let mut child_jobs = Vec::new();

    for entry in read.flatten() {
        let path = entry.path();
        let name = CompactString::from(entry.file_name().to_string_lossy());

        // lstat: never follow symlinks, so a symlink to a directory is counted as the link
        // itself and never traversed (matching ncdu's default and avoiding cycles).
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => {
                // Unreadable entry: record it with a read error, zero size.
                nodes.push(NewNode {
                    name,
                    kind: NodeKind::Other,
                    apparent: 0,
                    disk: 0,
                    dev: 0,
                    ino: 0,
                    hlink: false,
                    shared: false,
                    excluded: Excluded::No,
                    read_error: true,
                    dir_id: None,
                });
                continue;
            }
        };

        let ft = meta.file_type();
        let kind = if ft.is_dir() {
            NodeKind::Dir
        } else if ft.is_symlink() {
            NodeKind::Link
        } else if ft.is_file() {
            NodeKind::File
        } else {
            NodeKind::Other
        };
        let dev = meta.dev();
        let ino = meta.ino();
        let apparent = meta.len();
        let disk = meta.blocks() * 512;
        let mtime = meta.mtime();
        let hlink = matches!(kind, NodeKind::File) && meta.nlink() > 1;

        // Determine exclusion / whether to recurse.
        let mut excluded = Excluded::No;
        if name_excluded(&name, &shared.excludes) {
            excluded = Excluded::Pattern;
        } else if age_excluded(mtime, shared) {
            excluded = Excluded::Age;
        } else if shared.one_file_system && matches!(kind, NodeKind::Dir) && dev != shared.root_dev
        {
            excluded = Excluded::OtherFs;
        }

        // Hard-link dedup: a file whose (dev, ino) we've already counted is "shared" and
        // contributes zero to totals (so the grand total counts each inode once).
        let mut shared_hl = false;
        if hlink && excluded == Excluded::No {
            let mut seen = shared.seen_hardlinks.lock().unwrap();
            if !seen.insert((dev, ino)) {
                shared_hl = true;
            }
        }

        let recurse = matches!(kind, NodeKind::Dir) && excluded == Excluded::No;
        let dir_id = if recurse {
            Some(shared.next_id.fetch_add(1, Ordering::Relaxed))
        } else {
            None
        };

        nodes.push(NewNode {
            name,
            kind,
            apparent,
            disk,
            dev,
            ino,
            hlink,
            shared: shared_hl,
            excluded,
            read_error: false,
            dir_id,
        });
        if let Some(id) = dir_id {
            child_jobs.push(DirJob { id, path });
        }
    }

    // Publish this directory's children BEFORE queueing the subdirectories. This guarantees a
    // node's creation batch reaches the UI before any of its descendants' batches, so the tree
    // is always grafted parent-first.
    let _ = shared.ev_tx.send(Batch {
        parent_id: job.id,
        nodes,
        error: None,
    });

    // Enqueue the subdirectories, prioritizing those under the focused path, and wake idle
    // workers. The readdir/stat work above runs outside the lock; only queueing takes it.
    if !child_jobs.is_empty() {
        let (lock, cv) = &**frontier;
        let mut f = lock.lock().unwrap();
        f.pending += child_jobs.len();
        for cj in child_jobs {
            f.push(cj);
        }
        cv.notify_all();
    }
}

fn name_excluded(name: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|p| glob::matches(p, name))
}

/// Outside the `--older-than`/`--newer-than` window: `older_than` excludes entries modified
/// before its cutoff, `newer_than` excludes entries modified after it. Both flag the entry
/// like `--exclude` (visible, not descended into, not counted).
fn age_excluded(mtime: i64, shared: &Shared) -> bool {
    if let Some(cutoff) = shared.older_than {
        if mtime < cutoff {
            return true;
        }
    }
    if let Some(cutoff) = shared.newer_than {
        if mtime > cutoff {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Tree;
    use std::fs;

    fn opts(threads: usize, root_dev: u64) -> Opts {
        Opts {
            threads,
            one_file_system: false,
            excludes: Vec::new(),
            root_dev,
            older_than: None,
            newer_than: None,
        }
    }

    /// Scan a known tree and confirm the root aggregates every file's apparent size,
    /// every node is reachable, and out-of-order/orphan buffering never drops a batch.
    #[test]
    fn aggregates_full_tree() {
        // Build a deterministic temp tree (no external deps).
        let base = std::env::temp_dir().join(format!("rcdu_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("a/b/c")).unwrap();
        fs::create_dir_all(base.join("d")).unwrap();
        fs::write(base.join("a/f1"), vec![b'x'; 1000]).unwrap();
        fs::write(base.join("a/b/f2"), vec![b'x'; 2000]).unwrap();
        fs::write(base.join("a/b/c/f3"), vec![b'x'; 3000]).unwrap();
        fs::write(base.join("d/f4"), vec![b'x'; 4000]).unwrap();
        fs::write(base.join("top"), vec![b'x'; 500]).unwrap();
        let expected_files_bytes = 1000 + 2000 + 3000 + 4000 + 500;
        let expected_files = 5;
        let expected_dirs = 1 /*root*/ + 4 /*a,b,c,d*/;

        let meta = fs::symlink_metadata(&base).unwrap();
        let mut tree = Tree::new(
            "root".into(),
            meta.len(),
            meta.blocks() * 512,
            meta.dev(),
            meta.ino(),
        );

        let rx = start(base.clone(), opts(8, meta.dev())).events;
        for batch in rx.iter() {
            tree.apply(batch);
        }

        assert_eq!(tree.total_files, expected_files, "file count");
        assert_eq!(tree.total_dirs, expected_dirs, "dir count");
        assert!(tree.orphans_is_empty(), "no batch left buffered");
        // The whole tree is scanned, so the root subtree must report complete.
        assert!(
            tree.subtree_complete(tree.root),
            "subtree should be complete"
        );

        // The aggregated root size must equal the sum of every node's (derived) own size, for
        // both metrics — verifying the aggregation and the own-size derivation agree.
        let total_own_apparent: u64 = (0..tree.nodes.len()).map(|i| tree.own_apparent(i)).sum();
        let total_own_disk: u64 = (0..tree.nodes.len()).map(|i| tree.own_disk(i)).sum();
        assert_eq!(
            tree.nodes[tree.root].apparent, total_own_apparent,
            "apparent aggregation"
        );
        assert_eq!(
            tree.nodes[tree.root].disk, total_own_disk,
            "disk aggregation"
        );

        // The file bytes we wrote must be accounted for exactly (files are leaves: own == size).
        let file_bytes: u64 = (0..tree.nodes.len())
            .filter(|&i| !tree.nodes[i].is_dir())
            .map(|i| tree.own_apparent(i))
            .sum();
        assert_eq!(file_bytes, expected_files_bytes, "file bytes accounted");

        fs::remove_dir_all(&base).unwrap();
    }

    /// `--exclude` keeps an entry visible but stops recursion and zeroes its contribution.
    #[test]
    fn exclude_pattern_stops_recursion() {
        let base = std::env::temp_dir().join(format!("rcdu_excl_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("keep")).unwrap();
        fs::create_dir_all(base.join("node_modules/deep")).unwrap();
        fs::write(base.join("keep/f"), vec![b'x'; 1000]).unwrap();
        fs::write(base.join("node_modules/big"), vec![b'x'; 9_000_000]).unwrap();

        let meta = fs::symlink_metadata(&base).unwrap();
        let mut tree = Tree::new(
            "root".into(),
            meta.len(),
            meta.blocks() * 512,
            meta.dev(),
            meta.ino(),
        );
        let mut o = opts(4, meta.dev());
        o.excludes = vec!["node_modules".to_string()];
        let rx = start(base.clone(), o).events;
        for batch in rx.iter() {
            tree.apply(batch);
        }

        // node_modules exists as a node but was not recursed and contributes nothing.
        let nm = tree.nodes[tree.root]
            .children
            .iter()
            .find(|&&c| tree.nodes[c].name == "node_modules")
            .copied()
            .expect("node_modules present");
        assert!(tree.nodes[nm].children.is_empty(), "not recursed");
        assert_eq!(tree.nodes[nm].excluded, Excluded::Pattern);
        // The 9 MB inside node_modules must NOT be in the root total.
        assert!(
            tree.nodes[tree.root].apparent < 9_000_000,
            "excluded bytes counted"
        );

        fs::remove_dir_all(&base).unwrap();
    }

    /// Hard-linked files are counted once; the duplicate is flagged `shared`.
    #[test]
    fn hardlinks_counted_once() {
        let base = std::env::temp_dir().join(format!("rcdu_hl_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        fs::write(base.join("orig"), vec![b'x'; 5000]).unwrap();
        fs::hard_link(base.join("orig"), base.join("link")).unwrap();

        let meta = fs::symlink_metadata(&base).unwrap();
        let mut tree = Tree::new(
            "root".into(),
            meta.len(),
            meta.blocks() * 512,
            meta.dev(),
            meta.ino(),
        );
        let rx = start(base.clone(), opts(4, meta.dev())).events;
        for batch in rx.iter() {
            tree.apply(batch);
        }

        let shared_count = tree.nodes.iter().filter(|n| n.shared).count();
        assert_eq!(shared_count, 1, "exactly one of the two links is 'shared'");
        // The 5000 bytes are counted once, not twice.
        let file_bytes: u64 = tree
            .nodes
            .iter()
            .filter(|n| matches!(n.kind, NodeKind::File) && n.counts())
            .map(|n| n.apparent) // files are leaves, so apparent == own size
            .sum();
        assert_eq!(file_bytes, 5000, "hardlinked bytes counted once");

        fs::remove_dir_all(&base).unwrap();
    }

    /// The frontier drains hot (focused) jobs before cold ones, and `set_focus`'s re-partition
    /// promotes the matching subtree. (Deterministic test of the scheduling core.)
    #[test]
    fn frontier_prioritizes_focused_subtree() {
        let mut f = Frontier {
            focus: None,
            hot: VecDeque::new(),
            cold: VecDeque::new(),
            pending: 0,
            done: false,
        };
        f.push(DirJob {
            id: 1,
            path: "/r/A/x".into(),
        });
        f.push(DirJob {
            id: 2,
            path: "/r/B/y".into(),
        });
        f.push(DirJob {
            id: 3,
            path: "/r/A/z".into(),
        });

        // Focus on /r/B and re-partition (mirrors ScanControl::set_focus).
        f.focus = Some("/r/B".into());
        let mut all = std::mem::take(&mut f.hot);
        all.extend(std::mem::take(&mut f.cold));
        for j in all {
            f.push(j);
        }

        // The /r/B job comes out first; the /r/A jobs follow in FIFO order.
        assert_eq!(f.pop().unwrap().id, 2, "focused subtree first");
        assert_eq!(f.pop().unwrap().id, 1);
        assert_eq!(f.pop().unwrap().id, 3);
        assert!(f.pop().is_none());
    }

    /// End-to-end: with a large `slow/` branch and a small `target/` branch, focusing `target/`
    /// makes its subtree finish scanning before `slow/` does.
    #[test]
    fn focus_prioritizes_navigated_subtree() {
        let base = std::env::temp_dir().join(format!("rcdu_prio_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        // A big branch that takes a while, and a small branch we'll "navigate into".
        for d in 0..150 {
            let sd = base.join("slow").join(format!("s{d}"));
            fs::create_dir_all(&sd).unwrap();
            for i in 0..100 {
                fs::write(sd.join(format!("f{i}")), b"x").unwrap();
            }
        }
        for d in 0..15 {
            fs::create_dir_all(base.join("target").join(format!("t{d}"))).unwrap();
        }

        let meta = fs::symlink_metadata(&base).unwrap();
        let scan = start(base.clone(), opts(8, meta.dev()));
        // Prioritize the target subtree immediately, as if the user navigated into it.
        scan.control.set_focus(Some(base.join("target")));

        // Track which directory ids belong to each branch (batches arrive parent-first).
        let mut target_ids: HashSet<u64> = HashSet::new();
        let mut slow_ids: HashSet<u64> = HashSet::new();
        let (mut last_target, mut last_slow) = (0usize, 0usize);

        for (i, batch) in scan.events.iter().enumerate() {
            if target_ids.contains(&batch.parent_id) {
                last_target = i;
            }
            if slow_ids.contains(&batch.parent_id) {
                last_slow = i;
            }
            for n in &batch.nodes {
                if let Some(id) = n.dir_id {
                    if batch.parent_id == 0 {
                        if n.name == "target" {
                            target_ids.insert(id);
                        } else if n.name == "slow" {
                            slow_ids.insert(id);
                        }
                    } else if target_ids.contains(&batch.parent_id) {
                        target_ids.insert(id);
                    } else if slow_ids.contains(&batch.parent_id) {
                        slow_ids.insert(id);
                    }
                }
            }
        }

        assert!(last_target > 0 && last_slow > 0, "both branches scanned");
        assert!(
            last_target < last_slow,
            "focused 'target' subtree should finish before 'slow' (target@{last_target}, slow@{last_slow})"
        );

        fs::remove_dir_all(&base).unwrap();
    }

    /// Subtree rescan end-to-end: after the whole tree is scanned, `Tree::begin_refresh` tears
    /// down one subtree (keeping ancestor totals consistent) and `start_at` with a fresh id
    /// base re-scans it in place, picking up on-disk changes and restoring totals.
    #[test]
    fn subtree_rescan_restores_totals_and_picks_up_changes() {
        let base = std::env::temp_dir().join(format!("rcdu_rescan_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("target/inner")).unwrap();
        fs::create_dir_all(base.join("other")).unwrap();
        fs::write(base.join("target/f1"), vec![b'x'; 1000]).unwrap();
        fs::write(base.join("target/inner/f2"), vec![b'x'; 2000]).unwrap();
        fs::write(base.join("other/f3"), vec![b'x'; 4000]).unwrap();

        let meta = fs::symlink_metadata(&base).unwrap();
        let mut tree = Tree::new(
            "root".into(),
            meta.len(),
            meta.blocks() * 512,
            meta.dev(),
            meta.ino(),
        );
        let rx = start(base.clone(), opts(4, meta.dev())).events;
        for batch in rx.iter() {
            tree.apply(batch);
        }
        tree.finish_scan();

        let target = tree.nodes[tree.root]
            .children
            .iter()
            .find(|&&c| tree.nodes[c].name == "target")
            .copied()
            .expect("target present");
        let root_total_before = tree.nodes[tree.root].apparent;
        let target_agg_before = tree.nodes[target].apparent;
        let target_own_before = tree.own_apparent(target);
        let files_before = tree.total_files;
        assert!(tree.subtree_complete(target));

        // On-disk change while nobody is looking: a new file appears in the subtree.
        fs::write(base.join("target/inner/new"), vec![b'x'; 7000]).unwrap();

        // Tear the subtree down in the model, then rescan just that directory into it.
        let id_base = SCAN_ID_STRIDE;
        tree.begin_refresh(target, id_base);
        assert!(
            !tree.subtree_complete(target),
            "rescan in progress: subtree incomplete"
        );
        assert!(tree.nodes[target].children.is_empty(), "children detached");
        assert_eq!(
            tree.nodes[target].apparent, target_own_before,
            "aggregate reset to own size"
        );
        assert_eq!(
            tree.nodes[tree.root].apparent,
            root_total_before - target_agg_before + target_own_before,
            "ancestor total stays consistent across the reset"
        );

        let rx = start_at(base.join("target"), opts(4, meta.dev()), id_base).events;
        for batch in rx.iter() {
            tree.apply(batch);
        }
        assert!(tree.subtree_complete(target), "rescan completed");
        assert!(tree.orphans_is_empty(), "no batch left buffered");
        assert!(
            tree.nodes[tree.root].apparent > root_total_before,
            "rescan picked up the added bytes"
        );
        assert_eq!(tree.total_files, files_before + 1);
        // The untouched branch is unaffected.
        assert_eq!(tree.nodes[target].children.len(), 2, "f1 + inner");

        // Ground truth: the refreshed tree must agree exactly with an independent fresh scan
        // of the same on-disk state (this also absorbs directory-size drift from the new
        // file, which makes a hand-computed total unreliable).
        let mut fresh = Tree::new(
            "root".into(),
            meta.len(),
            meta.blocks() * 512,
            meta.dev(),
            meta.ino(),
        );
        let rx = start(base.clone(), opts(4, meta.dev())).events;
        for batch in rx.iter() {
            fresh.apply(batch);
        }
        fresh.finish_scan();
        assert_eq!(
            tree.nodes[tree.root].apparent, fresh.nodes[fresh.root].apparent,
            "root apparent matches a fresh scan"
        );
        assert_eq!(
            tree.nodes[tree.root].disk, fresh.nodes[fresh.root].disk,
            "root disk usage matches a fresh scan"
        );
        assert_eq!(tree.total_files, fresh.total_files);
        assert_eq!(tree.total_dirs, fresh.total_dirs);

        fs::remove_dir_all(&base).unwrap();
    }

    /// `--older-than` / `--newer-than` exclude entries by mtime: filtered entries stay
    /// visible but are not descended into and don't count toward totals.
    #[test]
    fn age_filters_exclude_entries_at_scan_time() {
        use std::time::{Duration, SystemTime};

        let base = std::env::temp_dir().join(format!("rcdu_age_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("olddir")).unwrap();
        fs::write(base.join("oldfile"), vec![b'x'; 1000]).unwrap();
        fs::write(base.join("newfile"), vec![b'x'; 2000]).unwrap();
        fs::write(base.join("olddir/inner"), vec![b'x'; 4000]).unwrap();

        // Age the old entries: 2000-01-01, well before any cutoff below. futimens works
        // through a read-only fd, which is also the only way to open a directory.
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(946_684_800);
        for path in ["oldfile", "olddir", "olddir/inner"] {
            fs::File::open(base.join(path))
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(old))
                .unwrap();
        }
        let meta = fs::symlink_metadata(&base).unwrap();
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let cutoff = now - 100 * 86_400; // 100 days ago

        let mut tree = Tree::new(
            "root".into(),
            meta.len(),
            meta.blocks() * 512,
            meta.dev(),
            meta.ino(),
        );
        let mut o = opts(4, meta.dev());
        o.older_than = Some(cutoff);
        let rx = start(base.clone(), o).events;
        for batch in rx.iter() {
            tree.apply(batch);
        }
        assert!(tree.orphans_is_empty());

        let find = |name: &str| {
            tree.nodes[tree.root]
                .children
                .iter()
                .find(|&&c| tree.nodes[c].name == name)
                .copied()
                .unwrap_or_else(|| panic!("{name} present"))
        };
        let old_file = find("oldfile");
        let new_file = find("newfile");
        let old_dir = find("olddir");
        assert_eq!(tree.nodes[old_file].excluded, Excluded::Age);
        assert_eq!(tree.nodes[new_file].excluded, Excluded::No);
        assert_eq!(tree.nodes[old_dir].excluded, Excluded::Age);
        // The old directory was not descended into, so its inner file never entered the tree.
        assert!(tree.nodes[old_dir].children.is_empty());
        // Only the fresh file's bytes count toward the root total.
        assert_eq!(tree.nodes[tree.root].apparent, 2000 + meta.len());

        // And the mirror: --newer-than keeps the old entries and excludes the fresh ones.
        let mut tree = Tree::new(
            "root".into(),
            meta.len(),
            meta.blocks() * 512,
            meta.dev(),
            meta.ino(),
        );
        let mut o = opts(4, meta.dev());
        o.newer_than = Some(cutoff);
        let rx = start(base.clone(), o).events;
        for batch in rx.iter() {
            tree.apply(batch);
        }
        let find = |name: &str| {
            tree.nodes[tree.root]
                .children
                .iter()
                .find(|&&c| tree.nodes[c].name == name)
                .copied()
                .unwrap()
        };
        assert_eq!(tree.nodes[find("oldfile")].excluded, Excluded::No);
        assert_eq!(tree.nodes[find("newfile")].excluded, Excluded::Age);
        assert_eq!(tree.nodes[find("olddir")].excluded, Excluded::No);

        fs::remove_dir_all(&base).unwrap();
    }
}
