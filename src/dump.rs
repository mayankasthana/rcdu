//! ncdu-compatible JSON import/export.
//!
//! Format (ncdu export version 1): a top-level array
//!     `[major, minor, metadata, root]`
//! where each directory is itself an array `[info, child, child, ...]` beginning with its own
//! info object, and files are plain info objects. Sizes are stored *per item* (`asize`/`dsize`
//! are the entry's own size); totals are recomputed on load.
//!
//! Export is written by hand (streaming, no intermediate `Value`) so it stays cheap on huge
//! trees. Import uses `serde_json` for robustness against the variety of dumps in the wild.

use std::collections::HashSet;
use std::io::{self, Read, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::model::{Excluded, NodeIdx, NodeKind, Tree};

const MAJOR: u32 = 1;
const MINOR: u32 = 2;

// ---- Export ---------------------------------------------------------------------------------

pub fn export<W: Write>(tree: &Tree, w: &mut W) -> io::Result<()> {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    write!(
        w,
        "[{MAJOR},{MINOR},{{\"progname\":\"rcdu\",\"progver\":\"{}\",\"timestamp\":{ts}}},",
        env!("CARGO_PKG_VERSION")
    )?;
    write_node(tree, tree.root, w)?;
    writeln!(w, "]")
}

fn write_node<W: Write>(tree: &Tree, idx: NodeIdx, w: &mut W) -> io::Result<()> {
    let n = &tree.nodes[idx];
    if n.is_dir() {
        w.write_all(b"[")?;
        write_item(tree, idx, w)?;
        for &c in &n.children {
            w.write_all(b",")?;
            write_node(tree, c, w)?;
        }
        w.write_all(b"]")
    } else {
        write_item(tree, idx, w)
    }
}

fn write_item<W: Write>(tree: &Tree, idx: NodeIdx, w: &mut W) -> io::Result<()> {
    let n = &tree.nodes[idx];
    write!(w, "{{\"name\":")?;
    write_json_string(&n.name, w)?;
    write!(
        w,
        ",\"asize\":{},\"dsize\":{},\"ino\":{},\"dev\":{}",
        tree.own_apparent(idx),
        tree.own_disk(idx),
        n.ino,
        n.dev
    )?;
    if n.hlink {
        write!(w, ",\"hlnkc\":true")?;
    }
    if matches!(n.kind, NodeKind::Link | NodeKind::Other) {
        write!(w, ",\"notreg\":true")?;
    }
    if n.read_error {
        write!(w, ",\"read_error\":true")?;
    }
    match n.excluded {
        Excluded::Pattern => write!(w, ",\"excluded\":\"pattern\"")?,
        Excluded::OtherFs => write!(w, ",\"excluded\":\"otherfs\"")?,
        Excluded::No => {}
    }
    w.write_all(b"}")
}

fn write_json_string<W: Write>(s: &str, w: &mut W) -> io::Result<()> {
    w.write_all(b"\"")?;
    for c in s.chars() {
        match c {
            '"' => w.write_all(b"\\\"")?,
            '\\' => w.write_all(b"\\\\")?,
            '\n' => w.write_all(b"\\n")?,
            '\r' => w.write_all(b"\\r")?,
            '\t' => w.write_all(b"\\t")?,
            c if (c as u32) < 0x20 => write!(w, "\\u{:04x}", c as u32)?,
            c => write!(w, "{c}")?,
        }
    }
    w.write_all(b"\"")
}

// ---- Import ---------------------------------------------------------------------------------

pub fn import<R: Read>(r: R) -> io::Result<Tree> {
    let v: Value = serde_json::from_reader(r)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("invalid JSON: {e}")))?;
    let top = v
        .as_array()
        .ok_or_else(|| bad("top level must be an array [major, minor, meta, root]"))?;
    let root_val = top
        .get(3)
        .ok_or_else(|| bad("missing root element (index 3)"))?;

    // The root must be a directory array: [info, children...].
    let arr = root_val
        .as_array()
        .ok_or_else(|| bad("root must be a directory array"))?;
    let info = arr.first().ok_or_else(|| bad("empty root directory"))?;

    let item = parse_item(info);
    let mut tree = Tree::new_imported(
        item.name.into(),
        item.apparent,
        item.disk,
        item.dev,
        item.ino,
    );
    let root = tree.root;

    // Dedup hard links during import too, so totals match a live scan.
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    for child in &arr[1..] {
        add_value(&mut tree, root, child, &mut seen);
    }
    Ok(tree)
}

struct Item {
    name: String,
    apparent: u64,
    disk: u64,
    dev: u64,
    ino: u64,
    hlink: bool,
    notreg: bool,
    read_error: bool,
    excluded: Excluded,
}

fn parse_item(v: &Value) -> Item {
    let g = |k: &str| v.get(k);
    Item {
        name: g("name").and_then(Value::as_str).unwrap_or("?").to_string(),
        apparent: g("asize").and_then(Value::as_u64).unwrap_or(0),
        disk: g("dsize").and_then(Value::as_u64).unwrap_or(0),
        dev: g("dev").and_then(Value::as_u64).unwrap_or(0),
        ino: g("ino").and_then(Value::as_u64).unwrap_or(0),
        hlink: g("hlnkc").and_then(Value::as_bool).unwrap_or(false),
        notreg: g("notreg").and_then(Value::as_bool).unwrap_or(false),
        read_error: g("read_error").and_then(Value::as_bool).unwrap_or(false),
        excluded: match g("excluded").and_then(Value::as_str) {
            Some("otherfs") => Excluded::OtherFs,
            Some(_) => Excluded::Pattern,
            None => Excluded::No,
        },
    }
}

fn add_value(tree: &mut Tree, parent: NodeIdx, v: &Value, seen: &mut HashSet<(u64, u64)>) {
    if let Some(arr) = v.as_array() {
        // Directory: [info, children...].
        let Some(info) = arr.first() else { return };
        let item = parse_item(info);
        let idx = tree.import_child(
            parent,
            item.name.into(),
            NodeKind::Dir,
            item.apparent,
            item.disk,
            item.dev,
            item.ino,
            item.hlink,
            false,
            item.excluded,
            item.read_error,
        );
        for child in &arr[1..] {
            add_value(tree, idx, child, seen);
        }
    } else {
        let item = parse_item(v);
        let kind = if item.notreg {
            NodeKind::Other
        } else {
            NodeKind::File
        };
        // Re-derive hard-link sharing so imported totals dedupe like a live scan.
        let shared = item.hlink && !seen.insert((item.dev, item.ino));
        tree.import_child(
            parent,
            item.name.into(),
            kind,
            item.apparent,
            item.disk,
            item.dev,
            item.ino,
            item.hlink,
            shared,
            item.excluded,
            item.read_error,
        );
    }
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_then_import_roundtrips_totals() {
        // Build a small tree by hand via the import path, export it, re-import, compare totals.
        let mut t = Tree::new_imported("/root".into(), 4096, 4096, 1, 2);
        let d = t.import_child(
            t.root,
            "sub".into(),
            NodeKind::Dir,
            4096,
            4096,
            1,
            3,
            false,
            false,
            Excluded::No,
            false,
        );
        t.import_child(
            d,
            "a".into(),
            NodeKind::File,
            1000,
            2048,
            1,
            4,
            false,
            false,
            Excluded::No,
            false,
        );
        t.import_child(
            t.root,
            "b".into(),
            NodeKind::File,
            500,
            512,
            1,
            5,
            false,
            false,
            Excluded::No,
            false,
        );
        let total = t.nodes[t.root].apparent;

        let mut buf = Vec::new();
        export(&t, &mut buf).unwrap();
        let t2 = import(&buf[..]).unwrap();

        assert_eq!(t2.nodes[t2.root].apparent, total);
        assert_eq!(t2.total_files, t.total_files);
        assert_eq!(t2.total_dirs, t.total_dirs);
        assert_eq!(t2.nodes[t2.root].name, "/root");
    }
}
