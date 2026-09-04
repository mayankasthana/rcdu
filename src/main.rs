//! rcdu — an ncdu-like disk usage explorer that is interactive while it scans.

mod app;
mod dump;
mod glob;
mod model;
mod scan;
mod ui;

use std::io::{BufReader, BufWriter, Write};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::time::Duration;

use crossbeam_channel::TryRecvError;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

use app::{App, KeyAction};
use model::Tree;
use scan::Opts;

struct Args {
    path: PathBuf,
    threads: usize,
    disk_usage: bool,
    one_file_system: bool,
    read_only: bool,
    excludes: Vec<String>,
    /// `-o FILE`: scan headless and write ncdu JSON, then exit. `-` means stdout.
    export: Option<String>,
    /// `-f FILE`: load a tree from ncdu JSON instead of scanning. `-` means stdin.
    import: Option<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut path: Option<PathBuf> = None;
    let mut threads = scan::default_threads();
    let mut disk_usage = true;
    let mut one_file_system = false;
    let mut read_only = false;
    let mut excludes = Vec::new();
    let mut export = None;
    let mut import = None;

    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            "-V" | "-v" | "--version" => {
                println!("rcdu {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "-t" | "--threads" => {
                let v = it.next().ok_or("--threads needs a value")?;
                threads = v
                    .parse::<usize>()
                    .map_err(|_| "--threads must be a number")?
                    .max(1);
            }
            "-a" | "--apparent-size" | "--apparent" => disk_usage = false,
            "-x" | "--one-file-system" => one_file_system = true,
            "-r" | "--read-only" => read_only = true,
            "--exclude" => {
                excludes.push(it.next().ok_or("--exclude needs a pattern")?);
            }
            "-X" | "--exclude-from" => {
                let file = it.next().ok_or("--exclude-from needs a file")?;
                let content = std::fs::read_to_string(&file).map_err(|e| format!("{file}: {e}"))?;
                for line in content.lines() {
                    let line = line.trim();
                    if !line.is_empty() && !line.starts_with('#') {
                        excludes.push(line.to_string());
                    }
                }
            }
            "-o" | "--output" => export = Some(it.next().ok_or("-o needs a file (or -)")?),
            "-f" | "--file" => import = Some(it.next().ok_or("-f needs a file (or -)")?),
            // Bundled boolean short flags, e.g. `-rx`.
            other if other.starts_with('-') && !other.starts_with("--") && other.len() > 2 => {
                for ch in other[1..].chars() {
                    match ch {
                        'a' => disk_usage = false,
                        'x' => one_file_system = true,
                        'r' => read_only = true,
                        _ => return Err(format!("unknown flag -{ch} in {other}")),
                    }
                }
            }
            other if other.starts_with('-') && other != "-" => {
                return Err(format!("unknown option: {other}"));
            }
            other => path = Some(PathBuf::from(other)),
        }
    }

    Ok(Args {
        path: path.unwrap_or_else(|| PathBuf::from(".")),
        threads,
        disk_usage,
        one_file_system,
        read_only,
        excludes,
        export,
        import,
    })
}

fn print_help() {
    println!(
        "rcdu {} — interactive, SSD-optimized disk usage explorer (ncdu-compatible)

USAGE:
    rcdu [PATH] [OPTIONS]

ARGS:
    PATH                     directory to scan (default: current directory)

OPTIONS:
    -t, --threads N          scanner threads (default: cores*4, tuned for SSDs)
    -a, --apparent-size      show apparent size instead of on-disk usage
    -x, --one-file-system    do not cross filesystem boundaries
        --exclude PATTERN    exclude entries matching a glob (repeatable)
    -X, --exclude-from FILE  read exclude patterns from FILE (one per line)
    -r, --read-only          disable the delete feature
    -o, --output FILE        scan without UI and write ncdu-compatible JSON ('-' = stdout)
    -f, --file FILE          load a tree from ncdu JSON instead of scanning ('-' = stdin)
    -h, --help               show this help
    -V, --version            show version

KEYS:
    j/k, down/up    move          l/Enter/right   enter directory
    h/Backspace     go up         s               toggle sort (size/name)
    a               apparent/disk d               delete (confirm; needs full scan)
    ?               help          g/G             top/bottom        q/Esc   quit",
        env!("CARGO_PKG_VERSION")
    );
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("rcdu: {e}\nTry 'rcdu --help'.");
            std::process::exit(2);
        }
    };

    // --- Export mode: produce a tree (by import or scan), write JSON, exit. No UI. ---
    if let Some(dest) = args.export.clone() {
        let tree = if let Some(src) = &args.import {
            // Convert/recompute: load a dump and write it back out.
            unwrap_or_die(load_import(src), src)
        } else {
            let (tree, root, opts) = prepare_scan(&args);
            scan_to_completion(tree, root, opts)
        };
        if let Err(e) = write_export(&tree, &dest) {
            eprintln!("rcdu: export failed: {e}");
            std::process::exit(1);
        }
        return;
    }

    // --- Import mode: load a dump, browse it, no scanning. ---
    if let Some(src) = &args.import {
        let tree = unwrap_or_die(load_import(src), src);
        let mut app = App::new(tree, args.disk_usage, args.read_only, false);
        run_ui(&mut app);
        return;
    }

    // --- Interactive scan mode. ---
    let (tree, root, opts) = prepare_scan(&args);
    let scan = scan::start(root, opts);
    let mut app = App::new(tree, args.disk_usage, args.read_only, true);
    app.attach_scan(scan.control);

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut app, &scan.events);
    ratatui::restore();

    if let Err(e) = result {
        eprintln!("rcdu: {e}");
        std::process::exit(1);
    }
}

fn unwrap_or_die<T>(res: std::io::Result<T>, ctx: &str) -> T {
    match res {
        Ok(v) => v,
        Err(e) => {
            eprintln!("rcdu: {ctx}: {e}");
            std::process::exit(1);
        }
    }
}

/// Stat the root and build the initial tree + scan options, exiting on a bad path.
fn prepare_scan(args: &Args) -> (Tree, PathBuf, Opts) {
    let root = match std::fs::canonicalize(&args.path) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("rcdu: {}: {e}", args.path.display());
            std::process::exit(1);
        }
    };
    let meta = match std::fs::symlink_metadata(&root) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("rcdu: {}: {e}", root.display());
            std::process::exit(1);
        }
    };
    if !meta.is_dir() {
        eprintln!("rcdu: {} is not a directory", root.display());
        std::process::exit(1);
    }
    let root_name = root.to_string_lossy();
    let tree = Tree::new(
        root_name.into(),
        meta.len(),
        meta.blocks() * 512,
        meta.dev(),
        meta.ino(),
    );
    let opts = Opts {
        threads: args.threads,
        one_file_system: args.one_file_system,
        excludes: args.excludes.clone(),
        root_dev: meta.dev(),
    };
    (tree, root, opts)
}

fn scan_to_completion(mut tree: Tree, root: PathBuf, opts: Opts) -> Tree {
    let scan = scan::start(root, opts);
    for batch in scan.events.iter() {
        tree.apply(batch);
    }
    tree.finish_scan();
    eprintln!(
        "rcdu: scanned {} dirs, {} files",
        tree.total_dirs, tree.total_files
    );
    tree
}

fn load_import(src: &str) -> std::io::Result<Tree> {
    if src == "-" {
        dump::import(BufReader::new(std::io::stdin().lock()))
    } else {
        dump::import(BufReader::new(std::fs::File::open(src)?))
    }
}

fn write_export(tree: &Tree, dest: &str) -> std::io::Result<()> {
    if dest == "-" {
        let stdout = std::io::stdout();
        let mut w = BufWriter::new(stdout.lock());
        dump::export(tree, &mut w)?;
        w.flush()
    } else {
        let mut w = BufWriter::new(std::fs::File::create(dest)?);
        dump::export(tree, &mut w)?;
        w.flush()
    }
}

fn run_ui(app: &mut App) {
    let mut terminal = ratatui::init();
    // No scanner channel in import mode: feed an already-disconnected receiver.
    let (_tx, rx) = crossbeam_channel::bounded::<scan::Batch>(0);
    drop(_tx);
    let result = run(&mut terminal, app, &rx);
    ratatui::restore();
    if let Err(e) = result {
        eprintln!("rcdu: {e}");
        std::process::exit(1);
    }
}

fn run(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    events: &crossbeam_channel::Receiver<scan::Batch>,
) -> std::io::Result<()> {
    loop {
        // Drain whatever the scanner produced since the last frame. Cap the batch so a huge
        // filesystem can't starve input handling between redraws.
        let mut applied = 0;
        loop {
            match events.try_recv() {
                Ok(batch) => {
                    app.apply(batch);
                    applied += 1;
                    if applied >= 20_000 {
                        break;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if app.scanning {
                        app.scanning = false;
                        // No more batches will arrive — free the scan-only bookkeeping.
                        app.tree.finish_scan();
                    }
                    break;
                }
            }
        }

        // Fold in the result of any background deletion.
        app.poll_delete();

        terminal.draw(|f| ui::render(f, app))?;

        // Poll faster while scanning or deleting so updates/spinners animate smoothly.
        let busy = app.scanning || app.is_deleting();
        let timeout = Duration::from_millis(if busy { 80 } else { 250 });
        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    if let Some(action) = decode_key(key.code, key.modifiers) {
                        app.on_key(action);
                    }
                }
            }
        }
        app.tick = app.tick.wrapping_add(1);

        if app.quit {
            return Ok(());
        }
    }
}

fn decode_key(code: KeyCode, mods: KeyModifiers) -> Option<KeyAction> {
    Some(match code {
        KeyCode::Char('q') | KeyCode::Esc => KeyAction::Quit,
        KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => KeyAction::Quit,
        KeyCode::Char('j') | KeyCode::Down => KeyAction::Down,
        KeyCode::Char('k') | KeyCode::Up => KeyAction::Up,
        KeyCode::Char('g') | KeyCode::Home => KeyAction::Top,
        KeyCode::Char('G') | KeyCode::End => KeyAction::Bottom,
        KeyCode::Char('l') | KeyCode::Enter | KeyCode::Right => KeyAction::Enter,
        KeyCode::Char('h') | KeyCode::Backspace | KeyCode::Left => KeyAction::Leave,
        KeyCode::Char('s') => KeyAction::ToggleSort,
        KeyCode::Char('a') => KeyAction::ToggleUsage,
        KeyCode::Char('d') => KeyAction::Delete,
        KeyCode::Char('o') => KeyAction::Open,
        KeyCode::Char('y') | KeyCode::Char('Y') => KeyAction::Confirm,
        KeyCode::Char('n') | KeyCode::Char('N') => KeyAction::Cancel,
        KeyCode::Char('?') => KeyAction::Help,
        _ => return None,
    })
}
