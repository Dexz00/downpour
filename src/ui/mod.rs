//! Terminal interface.
//!
//! The screen is built from a `Snapshot` — the UI never touches engine state,
//! it only reads the most recent snapshot. That keeps drawing off the
//! download's critical path.

mod render;

use crate::engine::{Command, Snapshot};
use crate::picker::{PickMode, Priority};
use crate::session::Session;
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::prelude::*;
use std::sync::Arc;
use std::time::Duration;

/// Redraw interval. 10 fps is smooth and doesn't fight the download for CPU.
const FRAME: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Overview,
    Peers,
    Files,
    Trackers,
}

impl Tab {
    const ALL: [Tab; 4] = [Tab::Overview, Tab::Peers, Tab::Files, Tab::Trackers];

    fn title(self) -> &'static str {
        match self {
            Tab::Overview => "Overview",
            Tab::Peers => "Peers",
            Tab::Files => "Files",
            Tab::Trackers => "Trackers",
        }
    }
}

/// Text box for pasting a magnet or path without leaving the interface.
struct InputBox {
    text: String,
    error: Option<String>,
}

struct App {
    tab: Tab,
    torrent: usize,
    /// Index selected in the current tab's list.
    selection: usize,
    /// `Some` while the user is typing a new torrent.
    input: Option<InputBox>,
    /// Files refused on open. They stay on screen until the first keypress.
    notices: Vec<String>,
    should_quit: bool,
}

impl Default for App {
    fn default() -> Self {
        App {
            tab: Tab::Overview,
            torrent: 0,
            selection: 0,
            input: None,
            notices: Vec::new(),
            should_quit: false,
        }
    }
}

// -- palette ----------------------------------------------------------------

const ACCENT: Color = Color::Rgb(126, 201, 255);
const OK: Color = Color::Rgb(126, 220, 140);
const WARNING: Color = Color::Rgb(240, 190, 100);
const BAD: Color = Color::Rgb(240, 120, 120);
const MUTED: Color = Color::Rgb(120, 128, 140);

pub async fn run(session: Arc<Session>, notices: Vec<String>) -> Result<()> {
    let mut terminal = ratatui::init();
    // Pasting a magnet sends dozens of characters at once; with bracketed
    // paste they arrive as a single event instead of one per key.
    let _ = crossterm::execute!(std::io::stdout(), event::EnableBracketedPaste);

    let mut app = App {
        notices,
        ..App::default()
    };

    let result = loop {
        let snaps = session.snapshots();
        if app.torrent >= snaps.len() {
            app.torrent = snaps.len().saturating_sub(1);
        }

        if let Err(e) = terminal.draw(|frame| render::draw(frame, &app, &snaps)) {
            break Err(e.into());
        }
        if let Err(e) = handle_events(&mut app, &session, &snaps).await {
            break Err(e);
        }
        if app.should_quit {
            break Ok(());
        }
    };

    let _ = crossterm::execute!(std::io::stdout(), event::DisableBracketedPaste);
    ratatui::restore();
    result
}

async fn handle_events(app: &mut App, session: &Arc<Session>, snaps: &[Snapshot]) -> Result<()> {
    if !event::poll(FRAME)? {
        return Ok(());
    }
    // Drain everything pending before redrawing: pasting text without
    // bracketed paste becomes a burst of keys, and one per frame would be slow.
    loop {
        match event::read()? {
            Event::Paste(text) => {
                app.notices.clear();
                if let Some(input) = &mut app.input {
                    input.text.push_str(text.trim());
                    input.error = None;
                } else {
                    // Pasting with the box closed opens the box with the contents.
                    app.input = Some(InputBox {
                        text: text.trim().to_string(),
                        error: None,
                    });
                }
            }
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                // The first key dismisses the notices and does nothing else:
                // no one should close an error without having had a chance to read it.
                if !app.notices.is_empty() {
                    app.notices.clear();
                } else if app.input.is_some() {
                    key_in_input(app, session, key);
                } else {
                    key_normal(app, session, snaps, key).await;
                }
            }
            _ => {}
        }
        if app.should_quit || !event::poll(Duration::ZERO)? {
            return Ok(());
        }
    }
}

/// Keys while the add-torrent box is open.
fn key_in_input(app: &mut App, session: &Arc<Session>, key: event::KeyEvent) {
    let Some(input) = &mut app.input else {
        return;
    };
    match key.code {
        KeyCode::Esc => app.input = None,
        KeyCode::Backspace => {
            input.text.pop();
            input.error = None;
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            input.text.clear();
            input.error = None;
        }
        KeyCode::Char(c) => {
            input.text.push(c);
            input.error = None;
        }
        KeyCode::Enter => {
            let target = input.text.trim().trim_matches('"').to_string();
            if target.is_empty() {
                app.input = None;
                return;
            }
            match crate::session::parse_source(&target) {
                Ok(source) => {
                    session.add(source);
                    app.input = None;
                    app.torrent = session.count().saturating_sub(1);
                }
                Err(e) => input.error = Some(format!("{e:#}")),
            }
        }
        _ => {}
    }
}

async fn key_normal(
    app: &mut App,
    session: &Arc<Session>,
    snaps: &[Snapshot],
    key: event::KeyEvent,
) {
    let current = app.torrent;
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.should_quit = true
        }

        KeyCode::Tab | KeyCode::Right => {
            let i = Tab::ALL.iter().position(|a| *a == app.tab).unwrap_or(0);
            app.tab = Tab::ALL[(i + 1) % Tab::ALL.len()];
            app.selection = 0;
        }
        KeyCode::BackTab | KeyCode::Left => {
            let i = Tab::ALL.iter().position(|a| *a == app.tab).unwrap_or(0);
            app.tab = Tab::ALL[(i + Tab::ALL.len() - 1) % Tab::ALL.len()];
            app.selection = 0;
        }
        KeyCode::Char(c @ '1'..='4') => {
            app.tab = Tab::ALL[c as usize - '1' as usize];
            app.selection = 0;
        }

        KeyCode::Down | KeyCode::Char('j') => app.selection += 1,
        KeyCode::Up | KeyCode::Char('k') => app.selection = app.selection.saturating_sub(1),
        KeyCode::Char('n') => {
            if !snaps.is_empty() {
                app.torrent = (app.torrent + 1) % snaps.len();
            }
        }

        KeyCode::Char('p') => {
            let paused = snaps
                .get(current)
                .is_some_and(|s| s.state_label == "paused");
            session.command(
                current,
                if paused {
                    Command::Resume
                } else {
                    Command::Pause
                },
            );
        }
        KeyCode::Char('s') => {
            let sequential = snaps.get(current).is_some_and(|s| s.sequential);
            session.command(
                current,
                Command::SetMode(if sequential {
                    PickMode::Rarest
                } else {
                    PickMode::Sequential
                }),
            );
        }
        KeyCode::Char('r') => session.command(current, Command::Recheck),

        // Priority of the selected file, in the files tab.
        KeyCode::Char(' ') if app.tab == Tab::Files => {
            if let Some(snap) = snaps.get(current)
                && let Some(file) = snap.files.get(app.selection)
            {
                let next = match file.priority {
                    Priority::Skip => Priority::Normal,
                    _ => Priority::Skip,
                };
                session.command(current, Command::SetFilePriority(app.selection, next));
            }
        }
        KeyCode::Char('+') | KeyCode::Char('=') if app.tab == Tab::Files => {
            if let Some(snap) = snaps.get(current)
                && let Some(file) = snap.files.get(app.selection)
            {
                let next = match file.priority {
                    Priority::Skip => Priority::Low,
                    Priority::Low => Priority::Normal,
                    _ => Priority::High,
                };
                session.command(current, Command::SetFilePriority(app.selection, next));
            }
        }
        KeyCode::Char('-') if app.tab == Tab::Files => {
            if let Some(snap) = snaps.get(current)
                && let Some(file) = snap.files.get(app.selection)
            {
                let next = match file.priority {
                    Priority::High => Priority::Normal,
                    Priority::Normal => Priority::Low,
                    _ => Priority::Skip,
                };
                session.command(current, Command::SetFilePriority(app.selection, next));
            }
        }

        // Add a torrent without leaving the interface.
        KeyCode::Char('a') | KeyCode::Insert => {
            app.input = Some(InputBox {
                text: String::new(),
                error: None,
            });
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::render::{bit_is_set, draw, piece_map, wrap_text};
    use super::*;
    use crate::engine::{FileInfo, PeerInfo};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn test_snapshot() -> Snapshot {
        Snapshot {
            name: "test-bundle".to_string(),
            info_hash: "0123456789abcdef0123456789abcdef01234567".to_string(),
            state_label: "downloading".to_string(),
            have_pieces: 8,
            total_pieces: 16,
            pieces: vec![0xff, 0x00],
            partials: vec![(8, 0.5)],
            downloaded: 8 * 1024 * 1024,
            uploaded: 2 * 1024 * 1024,
            wanted: 16 * 1024 * 1024,
            total_size: 16 * 1024 * 1024,
            download_rate: 3.0 * 1024.0 * 1024.0,
            upload_rate: 512.0 * 1024.0,
            eta: Some(125),
            connected: 2,
            candidates: 40,
            dht_nodes: 120,
            down_history: vec![1.0, 5.0, 3.0],
            up_history: vec![0.5, 1.0],
            peers: vec![PeerInfo {
                addr: "10.0.0.7:51413".parse().unwrap(),
                client: "other-client/1.0".to_string(),
                progress: 0.75,
                download_rate: 2.0 * 1024.0 * 1024.0,
                upload_rate: 0.0,
                choking_us: false,
                interested_in_us: true,
                we_choke: false,
                in_flight: 12,
                snubbed: false,
            }],
            files: vec![
                FileInfo {
                    path: "bundle/video.mkv".into(),
                    length: 12 * 1024 * 1024,
                    downloaded: 6 * 1024 * 1024,
                    priority: Priority::High,
                },
                FileInfo {
                    path: "bundle/extra.nfo".into(),
                    length: 4 * 1024 * 1024,
                    downloaded: 0,
                    priority: Priority::Skip,
                },
            ],
            ..Default::default()
        }
    }

    /// Draws a tab in a fake terminal and returns the screen text.
    fn render(tab: Tab, snaps: &[Snapshot]) -> String {
        let mut terminal = Terminal::new(TestBackend::new(110, 30)).unwrap();
        let app = App {
            tab,
            ..App::default()
        };
        terminal.draw(|frame| draw(frame, &app, snaps)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let width = buffer.area.width as usize;
        buffer
            .content()
            .chunks(width)
            .map(|line| line.iter().map(|c| c.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn draws_overview_tab() {
        let screen = render(Tab::Overview, &[test_snapshot()]);
        assert!(screen.contains("test-bundle"), "torrent title");
        assert!(screen.contains("downloading"));
        assert!(screen.contains("50.0%"), "8 of 16 MiB");
        assert!(screen.contains("3.00 MiB/s"), "download rate");
        assert!(screen.contains("2m 5s"), "time remaining");
        assert!(screen.contains("2 peers"));
        assert!(screen.contains("DHT 120 nodes"));
        assert!(screen.contains('█'), "piece map with full blocks");
    }

    #[test]
    fn draws_the_other_tabs() {
        let snaps = [test_snapshot()];

        let peers = render(Tab::Peers, &snaps);
        assert!(peers.contains("10.0.0.7:51413"));
        assert!(peers.contains("other-client"));
        assert!(peers.contains("75.0%"));
        assert!(peers.contains("free"), "peer is not choking us");

        let files = render(Tab::Files, &snaps);
        assert!(files.contains("video.mkv"));
        assert!(files.contains("HIGH"), "file priority");
        assert!(files.contains("skip"), "file marked to skip");
        assert!(files.contains("▶"), "cursor on the selection");

        let trackers = render(Tab::Trackers, &snaps);
        assert!(trackers.contains("0123456789abcdef"), "info_hash");
        assert!(trackers.contains("16 pieces"));
    }

    #[test]
    fn welcome_screen_explains_what_to_do() {
        let screen = render(Tab::Overview, &[]);
        assert!(screen.contains("bittorrent client"), "logo and subtitle");
        assert!(screen.contains("paste a magnet link"));
        assert!(screen.contains("drag .torrent files onto downpour.exe"));
    }

    #[test]
    #[ignore]
    fn view_screens() {
        let mut terminal = Terminal::new(TestBackend::new(78, 22)).unwrap();
        let show = |terminal: &mut Terminal<TestBackend>, app: &App, title: &str| {
            terminal.draw(|frame| draw(frame, app, &[])).unwrap();
            let buffer = terminal.backend().buffer().clone();
            let width = buffer.area.width as usize;
            println!("\n=== {title} ===");
            for line in buffer.content().chunks(width) {
                println!("{}", line.iter().map(|c| c.symbol()).collect::<String>());
            }
        };
        show(&mut terminal, &App::default(), "welcome");
        show(
            &mut terminal,
            &App {
                notices: vec![
                    "`broken.torrent` is not a valid .torrent: invalid digit found in string"
                        .into(),
                    "movie.torrent: The system cannot find the file specified. (os error 2)".into(),
                ],
                ..App::default()
            },
            "notices",
        );
    }

    #[test]
    fn rejected_file_appears_on_screen() {
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        let app = App {
            notices: vec!["broken.torrent: invalid bencode".into()],
            ..App::default()
        };
        terminal.draw(|frame| draw(frame, &app, &[])).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let width = buffer.area.width as usize;
        let screen: String = buffer
            .content()
            .chunks(width)
            .map(|line| line.iter().map(|c| c.symbol()).collect::<String>() + "\n")
            .collect();

        assert!(screen.contains("could not open 1 file"), "{screen}");
        assert!(screen.contains("broken.torrent"), "the name of the culprit");
        assert!(screen.contains("invalid bencode"), "and the reason");
        assert!(screen.contains("press any key to continue"));
    }

    /// Twenty bad files must not overflow the box nor hide the rest.
    #[test]
    fn many_notices_fit_in_the_box() {
        let mut terminal = Terminal::new(TestBackend::new(60, 14)).unwrap();
        let app = App {
            notices: (0..20).map(|i| format!("file-{i}.torrent: bad")).collect(),
            ..App::default()
        };
        terminal.draw(|frame| draw(frame, &app, &[])).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let width = buffer.area.width as usize;
        let screen: String = buffer
            .content()
            .chunks(width)
            .map(|line| line.iter().map(|c| c.symbol()).collect::<String>() + "\n")
            .collect();

        assert!(screen.contains("could not open 20 files"), "{screen}");
        assert!(screen.contains("… and"), "says how many were left out");
        // The footer is what teaches how to leave the box: it must not be the part cut off.
        assert!(screen.contains("press any key to continue"), "{screen}");
    }

    #[test]
    fn wraps_text_by_words() {
        assert_eq!(wrap_text("one two three", 20), ["one two three"]);
        assert_eq!(wrap_text("one two three", 8), ["one two", "three"]);
        // Single word larger than the line: cut it, don't hang.
        assert_eq!(wrap_text("magnetíssimo", 5), ["magne"]);
        assert_eq!(wrap_text("", 10), Vec::<String>::new());
        assert_eq!(wrap_text("anything", 0), Vec::<String>::new());
        // Accents count as one character.
        assert_eq!(wrap_text("ação ção", 4), ["ação", "ção"]);
    }

    /// Even without room for the logo, the screen must say what to do.
    #[test]
    fn welcome_screen_shrinks_without_breaking() {
        let mut terminal = Terminal::new(TestBackend::new(60, 5)).unwrap();
        let app = App::default();
        terminal.draw(|frame| draw(frame, &app, &[])).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let text: String = buffer.content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("drag"), "instruction survives the squeeze");
    }

    fn render_with_input(input: InputBox, snaps: &[Snapshot]) -> String {
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        let app = App {
            input: Some(input),
            ..App::default()
        };
        terminal.draw(|frame| draw(frame, &app, snaps)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let width = buffer.area.width as usize;
        buffer
            .content()
            .chunks(width)
            .map(|l| l.iter().map(|c| c.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn input_box_shows_text_and_error() {
        let screen = render_with_input(
            InputBox {
                text: "magnet:?xt=urn:btih:abc".to_string(),
                error: None,
            },
            &[],
        );
        assert!(screen.contains("add torrent"), "box title");
        assert!(screen.contains("magnet:?xt=urn:btih:abc"), "typed text");
        assert!(screen.contains("enter confirm"));

        let with_error = render_with_input(
            InputBox {
                text: "thing.torrent".to_string(),
                error: Some("could not open".to_string()),
            },
            &[],
        );
        assert!(with_error.contains("could not open"));
    }

    /// The box floats on top of the screen of a running torrent.
    #[test]
    fn input_box_appears_over_download() {
        let screen = render_with_input(
            InputBox {
                text: String::new(),
                error: None,
            },
            &[test_snapshot()],
        );
        assert!(screen.contains("test-bundle"), "the torrent stays behind");
        assert!(screen.contains("add torrent"), "and the box on top");
    }

    /// Text larger than the box shows the end, which is where the cursor is.
    #[test]
    fn long_input_scrolls_to_the_end() {
        let long_text = format!("magnet:?xt=urn:btih:{}END", "0".repeat(200));
        let screen = render_with_input(
            InputBox {
                text: long_text,
                error: None,
            },
            &[],
        );
        assert!(screen.contains("END"), "the end of the text stays visible");
    }

    #[test]
    fn draws_in_a_tight_terminal() {
        // A small terminal must not panic from a negative area.
        let mut terminal = Terminal::new(TestBackend::new(24, 8)).unwrap();
        let app = App::default();
        let snaps = [test_snapshot()];
        terminal
            .draw(|frame| draw(frame, &app, &snaps))
            .expect("drawing must not fail on a small screen");
    }

    #[test]
    fn reads_bitfield_bits() {
        let bits = [0b1010_0000u8, 0b0000_0001];
        assert!(bit_is_set(&bits, 0));
        assert!(!bit_is_set(&bits, 1));
        assert!(bit_is_set(&bits, 2));
        assert!(bit_is_set(&bits, 15));
        assert!(!bit_is_set(&bits, 99), "out of bounds does not panic");
    }

    #[test]
    fn map_condenses_pieces_into_cells() {
        let mut snap = Snapshot {
            total_pieces: 32,
            // First 8 pieces complete.
            pieces: vec![0xff, 0x00, 0x00, 0x00],
            ..Default::default()
        };
        // A 4x1 area means 8 pieces per cell.
        let area = Rect::new(0, 0, 4, 1);
        let p = piece_map(&snap, area);
        let text = format!("{p:?}");
        assert!(text.contains('█'), "full block for the complete pieces");
        assert!(text.contains('·'), "empty for the rest");

        snap.total_pieces = 0;
        let empty = piece_map(&snap, area);
        assert!(format!("{empty:?}").contains("metadata"));
    }
}
