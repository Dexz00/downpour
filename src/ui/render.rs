//! Rendering: building the screen from a Snapshot.

use super::{ACCENT, App, BAD, InputBox, MUTED, OK, Tab, WARNING};
use crate::engine::Snapshot;
use crate::picker::Priority;
use crate::stats::{bytes, eta, rate};
use ratatui::prelude::*;
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Gauge, List, ListItem, Padding, Paragraph, Row, Sparkline,
    Table, Tabs, Wrap,
};

// -- drawing ---------------------------------------------------------------

pub(super) fn draw(frame: &mut Frame, app: &App, snaps: &[Snapshot]) {
    let area = frame.area();
    let Some(snap) = snaps.get(app.torrent) else {
        draw_welcome(frame, area);
        if let Some(input) = &app.input {
            draw_input(frame, area, input);
        }
        draw_notices(frame, area, &app.notices);
        return;
    };

    let [header, body, footer] = Layout::vertical([
        Constraint::Length(6),
        Constraint::Min(6),
        Constraint::Length(1),
    ])
    .areas(area);

    draw_header(frame, header, snap, snaps.len(), app.torrent);

    let [tabs, content] = Layout::vertical([Constraint::Length(2), Constraint::Min(3)]).areas(body);
    draw_tabs(frame, tabs, app.tab);

    match app.tab {
        Tab::Overview => draw_overview(frame, content, snap),
        Tab::Peers => draw_peers(frame, content, snap),
        Tab::Files => draw_files(frame, content, snap, app.selection),
        Tab::Trackers => draw_trackers(frame, content, snap),
    }

    draw_footer(frame, footer, app.tab);

    if let Some(input) = &app.input {
        draw_input(frame, area, input);
    }
    draw_notices(frame, area, &app.notices);
}

/// Box with the files that could not be opened. With no terminal behind it,
/// this is the user's only chance to know why a download is missing.
fn draw_notices(frame: &mut Frame, area: Rect, notices: &[String]) {
    if notices.is_empty() {
        return;
    }
    let width = area.width.saturating_sub(8).clamp(24, 76);
    let usable = width.saturating_sub(4) as usize; // borders and side breathing room

    // The text is wrapped here, and not by `Wrap`, because the box height
    // depends on how many lines each notice becomes. 5 are left over for the
    // borders, the breathing room and the footer.
    let cap = (area.height.saturating_sub(5) as usize).max(1);
    let mut text: Vec<String> = Vec::new();
    let mut shown = 0;
    for notice in notices {
        let chunks = wrap_text(notice, usable);
        if !text.is_empty() && text.len() + chunks.len() > cap {
            break;
        }
        text.extend(chunks);
        shown += 1;
        if text.len() >= cap {
            break;
        }
    }
    text.truncate(cap);

    let mut lines: Vec<Line> = text
        .into_iter()
        .map(|line| Line::from(Span::styled(line, Style::new().fg(BAD))))
        .collect();
    if shown < notices.len() {
        lines.push(Line::from(Span::styled(
            format!("… and {} more", notices.len() - shown),
            Style::new().fg(MUTED),
        )));
    }
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "press any key to continue",
        Style::new().fg(MUTED),
    )));

    let height = (lines.len() + 2).min(area.height as usize) as u16;
    let box_area = centered_rect(area, width, height);
    frame.render_widget(ratatui::widgets::Clear, box_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(WARNING))
        .padding(Padding::horizontal(1))
        .title(Span::styled(
            if notices.len() == 1 {
                " could not open 1 file ".to_string()
            } else {
                format!(" could not open {} files ", notices.len())
            },
            Style::new().fg(WARNING).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(box_area);
    frame.render_widget(block, box_area);
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Wrap on words. A single word larger than the width is cut — that is the
/// case of a whole magnet pasted in, and there is nothing in it worth preserving.
pub(super) fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if current.is_empty() {
            current = word.chars().take(width).collect();
        } else if current.chars().count() + 1 + word.chars().count() <= width {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(std::mem::take(&mut current));
            current = word.chars().take(width).collect();
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// Block logo, drawn by hand to fit narrow terminals.
const LOGO: [&str; 5] = [
    "█▀▄ █▀█ █ █ █ █ █▀█ █▀█ █ █ █▀█",
    "█ █ █ █ █ █ █▄█ █▀  █ █ █ █ █▀▄",
    "█▄▀ ▀▀▀ ▀▄▀ █ ▀ ▀   ▀▀▀ ▀▀▀ ▀ ▀",
    "",
    "bittorrent client",
];

/// Screen shown while there is no torrent in the session.
fn draw_welcome(frame: &mut Frame, area: Rect) {
    let block = frame_block(" downpour ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height < 6 {
        frame.render_widget(
            Paragraph::new("drag a .torrent or press [a] to paste a magnet")
                .style(Style::new().fg(MUTED)),
            inner,
        );
        return;
    }

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::default());
    for (i, art) in LOGO.iter().enumerate() {
        let style = if i < 3 {
            Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(MUTED)
        };
        lines.push(Line::from(Span::styled(*art, style)).centered());
    }
    lines.push(Line::default());
    lines.push(Line::default());

    let shortcut = |key: &str, text: &str| {
        Line::from(vec![
            Span::styled(format!(" {key} "), Style::new().fg(Color::Black).bg(ACCENT)),
            Span::styled(format!("  {text}"), Style::new().fg(Color::Reset)),
        ])
        .centered()
    };

    lines.push(
        Line::from(Span::styled(
            "paste a magnet link — the box opens by itself",
            Style::new().fg(OK),
        ))
        .centered(),
    );
    lines.push(Line::default());
    lines.push(shortcut("a", "type a magnet or path"));
    lines.push(Line::default());
    lines.push(shortcut("q", "quit"));
    lines.push(Line::default());
    lines.push(
        Line::from(Span::styled(
            "drag .torrent files onto downpour.exe",
            Style::new().fg(MUTED),
        ))
        .centered(),
    );

    // Center vertically whatever fits.
    let leftover = inner.height.saturating_sub(lines.len() as u16) / 2;
    let target = Rect {
        y: inner.y + leftover,
        height: inner.height - leftover,
        ..inner
    };
    frame.render_widget(Paragraph::new(lines), target);
}

/// Floating box for typing or pasting a magnet/path.
fn draw_input(frame: &mut Frame, area: Rect, input: &InputBox) {
    let width = area.width.saturating_sub(8).clamp(20, 76);
    let height = if input.error.is_some() { 7 } else { 5 };
    let box_area = centered_rect(area, width, height);

    frame.render_widget(ratatui::widgets::Clear, box_area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ACCENT))
        .title(Span::styled(
            " add torrent ",
            Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(box_area);
    frame.render_widget(block, box_area);

    // Show the end of the text when it passes the box width.
    let visible = inner.width.saturating_sub(2) as usize;
    let text: String = if input.text.chars().count() > visible {
        input
            .text
            .chars()
            .skip(input.text.chars().count() - visible)
            .collect()
    } else {
        input.text.clone()
    };

    let mut lines = vec![
        Line::from(Span::styled(
            "paste the magnet or the .torrent path",
            Style::new().fg(MUTED),
        )),
        Line::from(vec![
            Span::styled("› ", Style::new().fg(ACCENT)),
            Span::raw(text),
            Span::styled("█", Style::new().fg(ACCENT)),
        ]),
    ];
    if let Some(error) = &input.error {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            error.chars().take(visible).collect::<String>(),
            Style::new().fg(BAD),
        )));
    }
    lines.push(Line::from(Span::styled(
        "enter confirm · esc cancel",
        Style::new().fg(MUTED),
    )));

    frame.render_widget(Paragraph::new(lines), inner);
}

/// Rectangle centered inside `area`.
fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

/// Takes the title by value so the `Block` is not tied to a temporary
/// `format!` at the call site.
fn frame_block(title: impl Into<String>) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(MUTED))
        .title(Span::styled(
            title.into(),
            Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))
}

fn draw_header(
    frame: &mut Frame,
    area: Rect,
    snap: &Snapshot,
    total_torrents: usize,
    index: usize,
) {
    let title = if total_torrents > 1 {
        format!(" {} ({}/{}) ", snap.name, index + 1, total_torrents)
    } else {
        format!(" {} ", snap.name)
    };
    let block = frame_block(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [state_row, bar_row, numbers_row] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(2),
    ])
    .areas(inner);

    // Row 1: state and flags.
    let state_color = match snap.state_label.as_str() {
        "seeding" => OK,
        "downloading" => ACCENT,
        "error" => BAD,
        "paused" => MUTED,
        _ => WARNING,
    };
    let mut parts = vec![
        Span::styled(
            format!("● {}", snap.state_label),
            Style::new().fg(state_color).add_modifier(Modifier::BOLD),
        ),
        Span::raw("   "),
    ];
    if let Some(error) = &snap.error {
        parts.push(Span::styled(error.clone(), Style::new().fg(BAD)));
    } else {
        parts.push(Span::styled(
            format!("{} peers", snap.connected),
            Style::new().fg(MUTED),
        ));
        parts.push(Span::styled(
            format!("  ({} queued)", snap.candidates),
            Style::new().fg(MUTED),
        ));
        if snap.dht_nodes > 0 {
            parts.push(Span::styled(
                format!("   DHT {} nodes", snap.dht_nodes),
                Style::new().fg(MUTED),
            ));
        }
        if snap.sequential {
            parts.push(Span::styled("   [sequential]", Style::new().fg(WARNING)));
        }
        if snap.endgame {
            parts.push(Span::styled("   [endgame]", Style::new().fg(WARNING)));
        }
    }
    frame.render_widget(Line::from(parts), state_row);

    // Row 2: progress bar.
    let target = snap.wanted.max(1);
    let fraction = (snap.downloaded as f64 / target as f64).clamp(0.0, 1.0);
    let label = if snap.state_label == "checking" {
        format!("checking… {:.0}%", snap.check_progress * 100.0)
    } else {
        format!(
            "{:.1}%  {} / {}",
            fraction * 100.0,
            bytes(snap.downloaded),
            bytes(snap.wanted)
        )
    };
    let shown = if snap.state_label == "checking" {
        snap.check_progress
    } else {
        fraction
    };
    frame.render_widget(
        Gauge::default()
            .ratio(shown.clamp(0.0, 1.0))
            .label(label)
            .gauge_style(Style::new().fg(if fraction >= 1.0 { OK } else { ACCENT }))
            .use_unicode(true),
        bar_row,
    );

    // Row 3: numbers.
    let numbers = Line::from(vec![
        Span::styled("↓ ", Style::new().fg(OK)),
        Span::styled(
            rate(snap.download_rate),
            Style::new().fg(OK).add_modifier(Modifier::BOLD),
        ),
        Span::raw("    "),
        Span::styled("↑ ", Style::new().fg(ACCENT)),
        Span::styled(rate(snap.upload_rate), Style::new().fg(ACCENT)),
        Span::raw("    "),
        Span::styled("left ", Style::new().fg(MUTED)),
        Span::raw(eta(snap.eta)),
        Span::raw("    "),
        Span::styled("uploaded ", Style::new().fg(MUTED)),
        Span::raw(bytes(snap.uploaded)),
        Span::raw("    "),
        Span::styled("pieces ", Style::new().fg(MUTED)),
        Span::raw(format!("{}/{}", snap.have_pieces, snap.total_pieces)),
    ]);
    frame.render_widget(Paragraph::new(numbers), numbers_row);
}

fn draw_tabs(frame: &mut Frame, area: Rect, current: Tab) {
    let index = Tab::ALL.iter().position(|a| *a == current).unwrap_or(0);
    let titles: Vec<Line> = Tab::ALL
        .iter()
        .enumerate()
        .map(|(i, a)| {
            Line::from(vec![
                Span::styled(format!("{} ", i + 1), Style::new().fg(MUTED)),
                Span::raw(a.title()),
            ])
        })
        .collect();
    frame.render_widget(
        Tabs::new(titles)
            .select(index)
            .highlight_style(Style::new().fg(ACCENT).add_modifier(Modifier::BOLD))
            .divider(Span::styled("│", Style::new().fg(MUTED))),
        area,
    );
}

fn draw_overview(frame: &mut Frame, area: Rect, snap: &Snapshot) {
    let [chart, map] =
        Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(area);

    // Speed chart.
    let [g_down, g_up] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(chart);
    sparkline(
        frame,
        g_down,
        &snap.down_history,
        " download ",
        OK,
        snap.download_rate,
    );
    sparkline(
        frame,
        g_up,
        &snap.up_history,
        " upload ",
        ACCENT,
        snap.upload_rate,
    );

    // Piece map.
    let block = frame_block(format!(
        " pieces  ({} of {}) ",
        snap.have_pieces, snap.total_pieces
    ));
    let inner = block.inner(map);
    frame.render_widget(block, map);
    frame.render_widget(piece_map(snap, inner), inner);
}

fn sparkline(
    frame: &mut Frame,
    area: Rect,
    history: &[f64],
    title: &str,
    color: Color,
    current: f64,
) {
    let block = frame_block(format!("{title}{} ", rate(current)));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 {
        return;
    }

    // The scale follows the recent peak, otherwise a fast download flattens
    // the chart as soon as the speed drops.
    let peak = history.iter().copied().fold(1.0f64, f64::max);
    let data: Vec<u64> = history
        .iter()
        .map(|v| ((v / peak) * 100.0) as u64)
        .collect();
    frame.render_widget(
        Sparkline::default()
            .data(&data)
            .max(100)
            .style(Style::new().fg(color)),
        inner,
    );
}

/// Draws the pieces as a block map: full, in progress or empty.
pub(super) fn piece_map(snap: &Snapshot, area: Rect) -> Paragraph<'static> {
    if snap.total_pieces == 0 || area.width == 0 || area.height == 0 {
        return Paragraph::new(Span::styled(
            "waiting for metadata…",
            Style::new().fg(MUTED),
        ));
    }

    let cells = (area.width as usize) * (area.height as usize);
    // Each screen cell represents a contiguous stretch of pieces.
    let per_cell = snap.total_pieces.div_ceil(cells).max(1);
    let in_progress: std::collections::HashMap<u32, f32> = snap.partials.iter().copied().collect();

    let mut lines = Vec::with_capacity(area.height as usize);
    let mut spans = Vec::with_capacity(area.width as usize);

    for cell in 0..cells {
        let start = cell * per_cell;
        if start >= snap.total_pieces {
            break;
        }
        let end = (start + per_cell).min(snap.total_pieces);

        let mut complete = 0;
        let mut partial = 0;
        for piece in start..end {
            if bit_is_set(&snap.pieces, piece) {
                complete += 1;
            } else if in_progress.contains_key(&(piece as u32)) {
                partial += 1;
            }
        }

        let total = end - start;
        let (glyph, color) = if complete == total {
            ('█', OK)
        } else if complete * 2 >= total {
            ('▓', OK)
        } else if complete > 0 {
            ('▒', ACCENT)
        } else if partial > 0 {
            ('░', WARNING)
        } else {
            ('·', MUTED)
        };
        spans.push(Span::styled(glyph.to_string(), Style::new().fg(color)));

        if spans.len() == area.width as usize {
            lines.push(Line::from(std::mem::take(&mut spans)));
        }
    }
    if !spans.is_empty() {
        lines.push(Line::from(spans));
    }
    Paragraph::new(lines)
}

pub(super) fn bit_is_set(bits: &[u8], index: usize) -> bool {
    bits.get(index / 8)
        .is_some_and(|b| b & (0x80 >> (index % 8)) != 0)
}

fn draw_peers(frame: &mut Frame, area: Rect, snap: &Snapshot) {
    let header = Row::new(vec![
        "address",
        "client",
        "progress",
        "↓",
        "↑",
        "in flight",
        "state",
    ])
    .style(Style::new().fg(MUTED).add_modifier(Modifier::BOLD));

    let lines: Vec<Row> = snap
        .peers
        .iter()
        .map(|peer| {
            let mut marks = Vec::new();
            if peer.snubbed {
                marks.push(Span::styled("snubbed ", Style::new().fg(BAD)));
            }
            if peer.choking_us {
                marks.push(Span::styled("choked ", Style::new().fg(WARNING)));
            } else {
                marks.push(Span::styled("free ", Style::new().fg(OK)));
            }
            if peer.interested_in_us {
                marks.push(Span::styled(
                    if peer.we_choke { "wants " } else { "serving " },
                    Style::new().fg(if peer.we_choke { MUTED } else { ACCENT }),
                ));
            }

            Row::new(vec![
                Cell::from(peer.addr.to_string()),
                Cell::from(peer.client.chars().take(18).collect::<String>()),
                Cell::from(format!("{:>5.1}%", peer.progress * 100.0)),
                Cell::from(Span::styled(rate(peer.download_rate), Style::new().fg(OK))),
                Cell::from(Span::styled(
                    rate(peer.upload_rate),
                    Style::new().fg(ACCENT),
                )),
                Cell::from(peer.in_flight.to_string()),
                Cell::from(Line::from(marks)),
            ])
        })
        .collect();

    let widths = [
        Constraint::Length(22),
        Constraint::Length(19),
        Constraint::Length(8),
        Constraint::Length(12),
        Constraint::Length(12),
        Constraint::Length(5),
        Constraint::Min(18),
    ];
    frame.render_widget(
        Table::new(lines, widths)
            .header(header)
            .block(frame_block(format!(
                " {} peers connected  ·  swarm: {} seeds / {} leechers ",
                snap.connected, snap.seeds_in_swarm, snap.peers_in_swarm
            ))),
        area,
    );
}

fn draw_files(frame: &mut Frame, area: Rect, snap: &Snapshot, selection: usize) {
    let selection = selection.min(snap.files.len().saturating_sub(1));
    let items: Vec<ListItem> = snap
        .files
        .iter()
        .enumerate()
        .map(|(i, file)| {
            let fraction = if file.length == 0 {
                1.0
            } else {
                file.downloaded as f64 / file.length as f64
            };
            let (priority_label, priority_color) = match file.priority {
                Priority::Skip => ("skip  ", MUTED),
                Priority::Low => ("low   ", MUTED),
                Priority::Normal => ("normal", Color::Reset),
                Priority::High => ("HIGH  ", WARNING),
            };
            let selected = i == selection;

            let bar_width = 20usize;
            let filled = (fraction * bar_width as f64).round() as usize;
            let bar: String = "█".repeat(filled) + &"░".repeat(bar_width - filled);

            let name_style = if file.priority == Priority::Skip {
                Style::new().fg(MUTED).add_modifier(Modifier::CROSSED_OUT)
            } else if selected {
                Style::new().add_modifier(Modifier::BOLD)
            } else {
                Style::new()
            };

            ListItem::new(Line::from(vec![
                Span::styled(if selected { "▶ " } else { "  " }, Style::new().fg(ACCENT)),
                Span::styled(priority_label.to_string(), Style::new().fg(priority_color)),
                Span::raw("  "),
                Span::styled(
                    bar,
                    Style::new().fg(if fraction >= 1.0 { OK } else { ACCENT }),
                ),
                Span::raw(format!(" {:>5.1}%  ", fraction * 100.0)),
                Span::styled(
                    format!("{:>10}  ", bytes(file.length)),
                    Style::new().fg(MUTED),
                ),
                Span::styled(file.path.display().to_string(), name_style),
            ]))
        })
        .collect();

    let empty = items.is_empty();
    frame.render_widget(
        List::new(items).block(frame_block(format!(" {} files ", snap.files.len()))),
        area,
    );
    if empty {
        let block = frame_block(" files ");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        frame.render_widget(
            Paragraph::new("waiting for metadata…").style(Style::new().fg(MUTED)),
            inner,
        );
    }
}

fn draw_trackers(frame: &mut Frame, area: Rect, snap: &Snapshot) {
    let [list, info] = Layout::vertical([Constraint::Min(4), Constraint::Length(7)]).areas(area);

    let lines: Vec<Row> = snap
        .trackers
        .iter()
        .map(|tracker| {
            let (label, color) = match tracker.state {
                crate::tracker::TrackerState::Ok => ("ok", OK),
                crate::tracker::TrackerState::Querying => ("querying", ACCENT),
                crate::tracker::TrackerState::Failed => ("failed", BAD),
                crate::tracker::TrackerState::Waiting => ("waiting", MUTED),
                crate::tracker::TrackerState::Disabled => ("disabled", MUTED),
            };
            Row::new(vec![
                Cell::from(tracker.url.chars().take(48).collect::<String>()),
                Cell::from(Span::styled(label, Style::new().fg(color))),
                Cell::from(tracker.seeders.to_string()),
                Cell::from(tracker.leechers.to_string()),
                Cell::from(tracker.peers.to_string()),
                Cell::from(if tracker.next_in > 0 {
                    format!("{}s", tracker.next_in)
                } else {
                    "—".to_string()
                }),
                Cell::from(
                    tracker
                        .message
                        .clone()
                        .unwrap_or_default()
                        .chars()
                        .take(40)
                        .collect::<String>(),
                ),
            ])
        })
        .collect();

    let widths = [
        Constraint::Min(30),
        Constraint::Length(12),
        Constraint::Length(7),
        Constraint::Length(9),
        Constraint::Length(7),
        Constraint::Length(8),
        Constraint::Min(20),
    ];
    frame.render_widget(
        Table::new(lines, widths)
            .header(
                Row::new(vec![
                    "tracker", "state", "seeds", "leechers", "peers", "next", "message",
                ])
                .style(Style::new().fg(MUTED).add_modifier(Modifier::BOLD)),
            )
            .block(frame_block(" trackers ")),
        list,
    );

    let text = vec![
        Line::from(vec![
            Span::styled("info_hash  ", Style::new().fg(MUTED)),
            Span::raw(snap.info_hash.clone()),
        ]),
        Line::from(vec![
            Span::styled("DHT        ", Style::new().fg(MUTED)),
            Span::raw(if snap.dht_nodes > 0 {
                format!(
                    "{} nodes ({} good) · {} queries sent",
                    snap.dht_nodes, snap.dht_good, snap.dht_queries
                )
            } else {
                "disabled".to_string()
            }),
        ]),
        Line::from(vec![
            Span::styled("listening  ", Style::new().fg(MUTED)),
            Span::raw(format!("port {}", snap.listen_port)),
        ]),
        Line::from(vec![
            Span::styled("size       ", Style::new().fg(MUTED)),
            Span::raw(format!(
                "{} in {} pieces",
                bytes(snap.total_size),
                snap.total_pieces
            )),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: true })
            .block(frame_block(" torrent ")),
        info,
    );
}

fn draw_footer(frame: &mut Frame, area: Rect, tab: Tab) {
    let mut shortcuts: Vec<(&str, &str)> = vec![
        ("q", "quit"),
        ("tab", "tab"),
        ("a", "add"),
        ("p", "pause"),
        ("s", "sequential"),
        ("r", "recheck"),
        ("n", "next"),
    ];
    if tab == Tab::Files {
        shortcuts.push(("space", "skip/include"));
        shortcuts.push(("+/-", "priority"));
    }

    let mut spans = Vec::new();
    for (key, action) in shortcuts {
        spans.push(Span::styled(
            format!(" {key} "),
            Style::new().fg(Color::Black).bg(ACCENT),
        ));
        spans.push(Span::styled(
            format!(" {action}   "),
            Style::new().fg(MUTED),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}
