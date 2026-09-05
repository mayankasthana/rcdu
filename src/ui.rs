//! Rendering. ncdu-style: a sorted list of the current directory's children, each with a size
//! bar, percentage of the parent, and a type indicator. Modal overlays (help, delete confirm)
//! are drawn on top.

use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{indicator, App, Modal, SortKey, TopFiles};
use crate::model::{Excluded, NodeKind};

const SPINNER: [&str; 4] = ["|", "/", "-", "\\"];

/// Human-readable size: powers of 1024, like ncdu.
pub fn fmt_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["  B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{:>6} {}", bytes, UNITS[u])
    } else {
        format!("{:>6.1} {}", v, UNITS[u])
    }
}

fn bar(frac: f64, width: usize) -> String {
    let filled = ((frac * width as f64).round() as usize).min(width);
    let mut s = String::with_capacity(width);
    for _ in 0..filled {
        s.push('#');
    }
    for _ in filled..width {
        s.push(' ');
    }
    s
}

pub fn render(f: &mut Frame, app: &App) {
    let chunks = Layout::vertical([
        Constraint::Length(2), // header
        Constraint::Min(1),    // list
        Constraint::Length(1), // footer
    ])
    .split(f.area());

    render_header(f, app, chunks[0]);
    render_list(f, app, chunks[1]);
    render_footer(f, app, chunks[2]);

    match &app.modal {
        Modal::Help => render_help(f),
        Modal::ConfirmDelete(idx) => render_confirm(f, app, *idx),
        Modal::TopFiles(tf) => render_top_files(f, app, tf),
        Modal::Info(idx) => render_info(f, app, *idx),
        Modal::None => {}
    }
}

/// The info popup: tree data plus a fresh on-disk stat of the selected entry, label/value
/// rows. Read-only; any key closes it.
fn render_info(f: &mut Frame, app: &App, idx: usize) {
    let rows = app.info_pairs(idx);
    let mut lines: Vec<Line> = rows
        .iter()
        .map(|(k, v)| {
            Line::from(vec![
                Span::styled(format!("  {k:<12} "), Style::default().fg(Color::Cyan)),
                Span::raw(v.as_str()),
            ])
        })
        .collect();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  press any key to close",
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC),
    )));
    let width = 74u16;
    let height = wrapped_height(&lines, width - 2) + 2;
    let area = popup(f.area(), width, height);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title(" Info ")),
        area,
    );
}

/// The top-files popup: largest regular files under the viewed directory, one row each, with
/// the selection highlighted. `j`/`k` move, `Enter` jumps to the file's parent, `q` closes.
fn render_top_files(f: &mut Frame, app: &App, tf: &TopFiles) {
    let width = f.area().width.saturating_sub(4).clamp(28, 96);
    let height = (tf.items.len() as u16 + 5)
        .min(f.area().height.saturating_sub(2))
        .max(7);
    let area = popup(f.area(), width, height);
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" Top files in {} ", app.tree.nodes[app.cur].name));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let [list_area, hint_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
    let metric = if app.disk_usage {
        "disk usage"
    } else {
        "apparent size"
    };
    let items: Vec<ListItem> = tf
        .items
        .iter()
        .map(|e| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{:>10}  ", fmt_size(e.size)),
                    Style::default().fg(Color::Green),
                ),
                Span::raw(e.path.clone()),
            ]))
        })
        .collect();
    let mut state = ListState::default();
    state.select(Some(tf.selected));
    f.render_stateful_widget(
        List::new(items).highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        list_area,
        &mut state,
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" j/k move · Enter go to file · q close   (sizes: {metric})"),
            Style::default().fg(Color::DarkGray),
        ))),
        hint_area,
    );
}

fn render_header(f: &mut Frame, app: &App, area: Rect) {
    let path = app.tree.path_of(app.cur);
    let total = app.size_of(app.cur);
    let metric = if app.disk_usage {
        "disk usage"
    } else {
        "apparent"
    };
    let l1 = Line::from(vec![
        Span::styled(
            "rcdu",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(path, Style::default().add_modifier(Modifier::BOLD)),
    ]);
    let own = app.own_size_of(app.cur);
    let l2 = Line::from(format!(
        "  total: {}  ({})   own: {}",
        fmt_size(total).trim_start(),
        metric,
        fmt_size(own).trim_start(),
    ));
    f.render_widget(Paragraph::new(vec![l1, l2]), area);
}

fn render_list(f: &mut Frame, app: &App, area: Rect) {
    let kids = app.sorted_children();
    let parent_total = app.size_of(app.cur).max(1);
    let max_child = kids
        .iter()
        .map(|&k| app.size_of(k))
        .max()
        .unwrap_or(1)
        .max(1);

    let mut items: Vec<ListItem> = Vec::with_capacity(kids.len());
    for &k in &kids {
        let n = &app.tree.nodes[k];
        let size = app.size_of(k);
        let pct = size as f64 / parent_total as f64 * 100.0;
        let frac = size as f64 / max_child as f64;

        let ind = indicator(n.kind, n.shared, n.excluded != Excluded::No, n.read_error);
        let name_style = if n.read_error || n.excluded != Excluded::No {
            Style::default().fg(Color::DarkGray)
        } else if n.shared {
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC)
        } else {
            match n.kind {
                NodeKind::Dir => Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
                NodeKind::Link => Style::default().fg(Color::Magenta),
                NodeKind::Other => Style::default().fg(Color::Yellow),
                NodeKind::File => Style::default(),
            }
        };

        // The subtree being deleted is locked: show a spinner and dim it. A subtree being
        // rescanned shows a spinner too, but stays styled normally (it's readable, not locked).
        let (name_style, suffix) = if app.deleting_idx() == Some(k) {
            (
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                format!("  [{} deleting…]", SPINNER[app.tick % SPINNER.len()]),
            )
        } else if app.refreshing_idx == Some(k) {
            (
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
                format!("  [{} rescanning…]", SPINNER[app.tick % SPINNER.len()]),
            )
        } else {
            let suffix = match n.excluded {
                Excluded::OtherFs => "  <other filesystem>".to_string(),
                Excluded::Pattern => "  <excluded>".to_string(),
                Excluded::No => String::new(),
            };
            (name_style, suffix)
        };

        let line = Line::from(vec![
            Span::styled(
                format!("{:>10} ", fmt_size(size)),
                Style::default().fg(Color::Green),
            ),
            Span::styled(
                format!("[{}] ", bar(frac, 10)),
                Style::default().fg(Color::DarkGray),
            ),
            Span::raw(format!("{pct:>5.1}% ")),
            Span::styled(format!("{ind}{}{suffix}", n.name), name_style),
        ]);
        items.push(ListItem::new(line));
    }

    let mut state = ListState::default();
    if let Some(sel) = app.selected {
        if let Some(pos) = kids.iter().position(|&k| k == sel) {
            state.select(Some(pos));
        }
    }

    let title = if app.tree.nodes[app.cur].children.is_empty() {
        if app.scanning {
            " reading… ".to_string()
        } else {
            " (empty) ".to_string()
        }
    } else if app.filter.is_some() {
        // When a filter is active, show how many of the directory's entries match.
        let total = app.tree.nodes[app.cur].children.len();
        format!(" {} of {} items ", kids.len(), total)
    } else {
        format!(" {} items ", kids.len())
    };

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::TOP | Borders::BOTTOM)
                .title(title),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    f.render_stateful_widget(list, area, &mut state);
}

fn render_footer(f: &mut Frame, app: &App, area: Rect) {
    // A transient status message wins (it carries warnings like the armed force-quit prompt).
    if let Some(msg) = &app.status {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {msg}"),
                Style::default().fg(Color::Black).bg(Color::Yellow),
            ))),
            area,
        );
        return;
    }

    // While a filter is being typed, the input line replaces the hints.
    if app.searching {
        let q = app.filter.as_deref().unwrap_or("");
        let line = Line::from(vec![
            Span::styled(
                " filter ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(" /{q}▌")),
            Span::styled(
                "  Enter apply · Esc cancel",
                Style::default().fg(Color::DarkGray),
            ),
        ]);
        f.render_widget(Paragraph::new(line), area);
        return;
    }

    // Otherwise a running deletion is the most important thing to surface; show it but keep the
    // key hints so it's clear browsing still works.
    if let Some(didx) = app.deleting_idx() {
        let name = &app.tree.nodes[didx].name;
        let line = Line::from(vec![
            Span::styled(
                " j/k move  l enter  h up  q quit ",
                Style::default().fg(Color::Yellow),
            ),
            Span::raw(" │ "),
            Span::styled(
                format!("{} deleting {name} …", SPINNER[app.tick % SPINNER.len()]),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
        ]);
        f.render_widget(Paragraph::new(line), area);
        return;
    }

    let status = if app.scanning {
        let rate = app
            .scan_rate()
            .map(|b| format!("  {}/s", fmt_size(b).trim_start()))
            .unwrap_or_default();
        format!(
            "{} scanning{rate}  {} dirs  {} files",
            SPINNER[app.tick % SPINNER.len()],
            app.tree.total_dirs,
            app.tree.total_files,
        )
    } else {
        format!(
            "done  {} dirs  {} files",
            app.tree.total_dirs, app.tree.total_files
        )
    };
    // An in-place rescan is worth surfacing even after the initial scan finished.
    let status = if let Some(idx) = app.refreshing_idx {
        format!(
            "{status}  {} rescanning {}…",
            SPINNER[app.tick % SPINNER.len()],
            app.tree.nodes[idx].name
        )
    } else {
        status
    };
    let sort = match app.sort {
        SortKey::Size => "size",
        SortKey::Name => "name",
    };
    let mut help = format!(" ? help  d delete  s sort:{sort}  a usage  q quit");
    if let Some(q) = &app.filter {
        // Keep the applied filter visible (with match counts) so it's never silently on.
        let matches = app.sorted_children().len();
        let total = app.tree.nodes[app.cur].children.len();
        help.push_str(&format!("  │  /{q} ({matches}/{total})"));
    }
    help.push_str(&format!("   │   {status}"));
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            help,
            Style::default().fg(Color::Yellow),
        ))),
        area,
    );
}

/// Center a popup of the given size within `area`, clamped to fit.
fn popup(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    let [v] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(area);
    let [h] = Layout::horizontal([Constraint::Length(width)])
        .flex(Flex::Center)
        .areas(v);
    h
}

/// Rows a set of lines needs at a given inner width, accounting for wrapping.
fn wrapped_height(lines: &[Line], inner_width: u16) -> u16 {
    let iw = inner_width.max(1) as usize;
    lines
        .iter()
        .map(|l| l.width().max(1).div_ceil(iw) as u16)
        .sum()
}

fn render_help(f: &mut Frame) {
    let lines = vec![
        Line::from(vec![Span::styled(
            "rcdu — keys",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )]),
        Line::from(""),
        Line::from("  j / k, ↓ / ↑      move selection"),
        Line::from("  l / Enter / →     enter directory"),
        Line::from("  h / Backspace / ← go up"),
        Line::from("  g / G             jump to top / bottom"),
        Line::from("  PgUp / PgDn, C-d / C-u  page up / down"),
        Line::from("  s                 toggle sort (size ↔ name)"),
        Line::from("  /                 filter entries by name (Enter applies, Esc cancels)"),
        Line::from("  a                 toggle apparent ↔ disk usage"),
        Line::from("  d                 delete selected (confirm; needs full scan)"),
        Line::from("  o                 open selected in default app / file manager"),
        Line::from("  t                 largest files under this directory"),
        Line::from("  i                 show details of the selected entry"),
        Line::from("  r                 rescan selected directory in place"),
        Line::from("  e                 export the tree to an ncdu-format JSON file"),
        Line::from("  x / !             jump to the next read-error entry"),
        Line::from("  ?                 toggle this help"),
        Line::from("  q / Esc / Ctrl-C  quit (during a delete: press twice to abort it)"),
        Line::from(""),
        Line::from(vec![Span::styled(
            "  indicators: / dir   @ link   H hard-link dup   < excluded   ! error",
            Style::default().fg(Color::DarkGray),
        )]),
        Line::from(""),
        Line::from(vec![Span::styled(
            "  press any key to close",
            Style::default().add_modifier(Modifier::ITALIC),
        )]),
    ];
    let width = 74u16;
    let height = wrapped_height(&lines, width - 2) + 2;
    let area = popup(f.area(), width, height);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title(" Help ")),
        area,
    );
}

fn render_confirm(f: &mut Frame, app: &App, idx: usize) {
    let n = &app.tree.nodes[idx];
    let path = app.tree.path_of(idx);
    let what = if n.is_dir() {
        format!(
            "directory and ALL its contents ({})",
            fmt_size(app.size_of(idx)).trim_start()
        )
    } else {
        format!("file ({})", fmt_size(app.size_of(idx)).trim_start())
    };
    let lines = vec![
        Line::from(vec![Span::styled(
            "Delete?",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )]),
        Line::from(""),
        Line::from(format!("  {path}")),
        Line::from(format!("  This removes the {what} from disk.")),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "  y",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" confirm    "),
            Span::styled("n / Esc", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" cancel"),
        ]),
    ];
    let width = 72.min(f.area().width.saturating_sub(4)).max(24);
    let height = wrapped_height(&lines, width - 2) + 2;
    let area = popup(f.area(), width, height);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Red))
                .title(" Confirm deletion "),
        ),
        area,
    );
}
