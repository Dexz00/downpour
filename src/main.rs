//! torrra — a terminal BitTorrent client.

use anyhow::{Context, Result, bail};
use clap::Parser;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use torrra::engine::Config;
use torrra::session::Session;
use torrra::{metainfo, platform, session, stats, ui};

#[derive(Parser)]
#[command(
    name = "torrra",
    version,
    about = "Terminal BitTorrent client",
    long_about = "Downloads .torrent files and magnet links, with DHT, PEX and a terminal interface."
)]
struct Args {
    /// .torrent files or magnet links. With none, the interface opens empty
    /// and you add one with the `a` key. On Windows, drag files onto the
    /// executable.
    #[arg(value_name = "TORRENT|MAGNET")]
    sources: Vec<String>,

    /// Destination directory (default: the Downloads folder when opened from
    /// a file manager, the current directory from a terminal)
    #[arg(short = 'o', long = "output", alias = "saida", value_name = "DIR")]
    output: Option<PathBuf>,

    /// Listening port (TCP and UDP)
    #[arg(short = 'p', long = "port", alias = "porta", default_value_t = 6881)]
    port: u16,

    /// Maximum connected peers per torrent
    #[arg(long, default_value_t = 100, value_name = "N")]
    max_peers: usize,

    /// How many peers we serve at the same time
    #[arg(long, default_value_t = 6, value_name = "N")]
    slots_upload: usize,

    /// Download pieces in order, to watch while downloading
    #[arg(short = 's', long = "sequential", alias = "sequencial")]
    sequential: bool,

    /// Disable DHT (trackers only)
    #[arg(long = "no-dht", alias = "sem-dht")]
    no_dht: bool,

    /// Exit as soon as the download completes, instead of seeding
    #[arg(long = "no-seed", alias = "sem-seed")]
    no_seed: bool,

    /// Download limit, e.g. 2M, 500k
    #[arg(
        long = "download-limit",
        alias = "limite-download",
        value_name = "RATE"
    )]
    download_limit: Option<String>,

    /// Upload limit, e.g. 1M
    #[arg(long = "upload-limit", alias = "limite-upload", value_name = "RATE")]
    upload_limit: Option<String>,

    /// No interface: just print progress, one line per second
    #[arg(long = "simple", alias = "simples")]
    simple: bool,

    /// Show the contents of a .torrent and exit
    #[arg(long, value_name = "TORRENT")]
    info: Option<PathBuf>,
}

fn main() -> Result<()> {
    platform::prepare_console();

    // `parse` would exit on the spot on error — and when opened from
    // Explorer that closes the window before the message can be read.
    let args = match Args::try_parse() {
        Ok(args) => args,
        Err(e) => {
            let _ = e.print();
            platform::hold_window();
            std::process::exit(e.exit_code());
        }
    };

    let result = run_all(args);
    if let Err(e) = &result {
        eprintln!("error: {e:#}");
        platform::hold_window();
    }
    result
}

fn run_all(args: Args) -> Result<()> {
    if let Some(path) = &args.info {
        return show_info(path);
    }

    // The runtime is built by hand to give the threads a useful name and to
    // size the blocking I/O pool, which is where piece verification runs.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("torrra")
        .max_blocking_threads(16)
        .build()?;
    runtime.block_on(run(args))
}

/// Where files end up, in precedence order: the `-o` option, a
/// `destination.txt` next to the executable, the Downloads folder (when the
/// program was opened from a file manager) and, finally, the current
/// directory.
///
/// The current directory is a poor default for a double click: Explorer
/// hands us the executable's own directory, and nobody wants the download
/// inside the program folder.
fn destination(requested: Option<PathBuf>) -> PathBuf {
    let downloads = platform::opened_by_file_manager()
        .then(platform::downloads_folder)
        .flatten();
    choose_destination(requested, pinned_destination(), downloads)
}

fn choose_destination(
    requested: Option<PathBuf>,
    pinned: Option<PathBuf>,
    downloads: Option<PathBuf>,
) -> PathBuf {
    requested
        .or(pinned)
        .or(downloads)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The message the user sees when a file cannot be opened.
///
/// Dragged from Explorer, the path arrives in full — and it alone fills the
/// notice box, pushing out the actual reason. The file name replaces the
/// path, and the name goes first if the error does not mention it.
fn rejection_notice(source: &str, e: &anyhow::Error) -> String {
    let name = Path::new(source)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| source.to_string());
    let reason = format!("{e:#}").replace(source, &name);
    if reason.contains(&name) {
        reason
    } else {
        format!("{name}: {reason}")
    }
}

/// `canonicalize` returns the path with the `\\?\` prefix on Windows. It is
/// legitimate, but alarming for someone who only wants to know where the
/// file ended up.
fn display_path(path: &Path) -> String {
    let text = path.display().to_string();
    text.strip_prefix(r"\\?\").unwrap_or(&text).to_string()
}

/// A `destination.txt` next to the executable, with the path on one line,
/// pins the destination folder for people who will never use a command
/// line. The legacy Portuguese name `destino.txt` is still honored.
fn pinned_destination() -> Option<PathBuf> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    for name in ["destination.txt", "destino.txt"] {
        if let Ok(contents) = std::fs::read_to_string(dir.join(name))
            && let Some(line) = contents.lines().next().map(str::trim)
            && !line.is_empty()
        {
            return Some(PathBuf::from(line));
        }
    }
    None
}

async fn run(args: Args) -> Result<()> {
    let cfg = Config {
        dest: destination(args.output.clone()),
        max_peers: args.max_peers.clamp(4, 1000),
        upload_slots: args.slots_upload.clamp(1, 50),
        listen_port: args.port,
        sequential: args.sequential,
        max_download: parse_rate(args.download_limit.as_deref())?,
        max_upload: parse_rate(args.upload_limit.as_deref())?,
        use_dht: !args.no_dht,
        seed_after_complete: !args.no_seed,
    };

    std::fs::create_dir_all(&cfg.dest)
        .with_context(|| format!("could not create {}", cfg.dest.display()))?;
    // Only once it exists can `.` and links in the path be resolved.
    let where_to = std::fs::canonicalize(&cfg.dest).unwrap_or_else(|_| cfg.dest.clone());

    let session = Session::start(cfg).await?;
    if args.simple {
        println!("destination: {}", display_path(&where_to));
        println!("listening on port {}", session.port());
    }

    // One invalid file among many (typical when dragging a handful of things
    // onto the executable) must not take down the good ones.
    let mut rejected = Vec::new();
    for source in &args.sources {
        match session::parse_source(source) {
            Ok(parsed) => {
                session.add(parsed);
            }
            Err(e) => rejected.push(rejection_notice(source, &e)),
        }
    }
    if args.simple {
        for notice in &rejected {
            eprintln!("skipped — {notice}");
        }
        if session.count() == 0 {
            bail!("no valid torrent to download");
        }
    }
    // In the interface the notices cannot go to stderr: it owns the whole
    // screen, and when opened from Explorer there is no terminal left to
    // read them in afterwards.

    // Ctrl+C must warn the trackers before exiting, otherwise the torrent
    // stays marked as active on their side for another half hour.
    let s = Arc::clone(&session);
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            ratatui::restore();
            s.shutdown().await;
            std::process::exit(0);
        }
    });

    let result = if args.simple {
        follow_simple(&session).await
    } else {
        ui::run(Arc::clone(&session), rejected).await
    };

    session.shutdown().await;
    result
}

/// Headless mode: one progress line per second.
async fn follow_simple(session: &Arc<Session>) -> Result<()> {
    use stats::{bytes, eta, rate};
    let mut tick = tokio::time::interval(Duration::from_secs(1));

    loop {
        tick.tick().await;
        let snaps = session.snapshots();
        let mut all_done = !snaps.is_empty();

        for snap in &snaps {
            let target = snap.wanted.max(1);
            let fraction = (snap.downloaded as f64 / target as f64).clamp(0.0, 1.0);
            all_done &= snap.state_label == "seeding" || fraction >= 1.0;

            println!(
                "[{:>5.1}%] {} · {} of {} · ↓{} ↑{} · {} peers · {} left{}",
                fraction * 100.0,
                truncate_text(&snap.name, 32),
                bytes(snap.downloaded),
                bytes(snap.wanted),
                rate(snap.download_rate),
                rate(snap.upload_rate),
                snap.connected,
                eta(snap.eta),
                snap.error
                    .as_ref()
                    .map(|e| format!(" · ERROR: {e}"))
                    .unwrap_or_default(),
            );
        }

        if all_done {
            println!("Done.");
            return Ok(());
        }
    }
}

fn show_info(path: &PathBuf) -> Result<()> {
    use stats::bytes;
    let meta = metainfo::Metainfo::from_file(path)?;
    let info = &meta.info;

    println!("name         {}", info.name);
    println!("info_hash    {}", info.info_hash);
    println!(
        "size         {} ({} bytes)",
        bytes(info.total_length),
        info.total_length
    );
    println!(
        "pieces       {} of {}",
        info.num_pieces(),
        bytes(info.piece_length as u64)
    );
    println!("private      {}", if info.private { "yes" } else { "no" });
    if let Some(c) = &meta.comment {
        println!("comment      {c}");
    }
    if let Some(c) = &meta.created_by {
        println!("created by   {c}");
    }
    if let Some(t) = meta.creation_date {
        println!("created on   {}", readable_date(t));
    }
    if !meta.web_seeds.is_empty() {
        println!("web seeds    {}", meta.web_seeds.join(", "));
    }

    println!("\ntrackers");
    if meta.tiers.is_empty() {
        println!("  (none — DHT only)");
    }
    for (i, tier) in meta.tiers.iter().enumerate() {
        for url in tier {
            println!("  [{i}] {url}");
        }
    }

    println!("\nfiles ({})", info.files.len());
    for file in &info.files {
        println!("  {:>12}  {}", bytes(file.length), file.path.display());
    }
    Ok(())
}

/// Converts `2M`, `500k`, `1024` into bytes per second.
fn parse_rate(text: Option<&str>) -> Result<Option<u64>> {
    let Some(text) = text else {
        return Ok(None);
    };
    let text = text.trim();
    let (number, factor) = match text.chars().last() {
        Some('k' | 'K') => (&text[..text.len() - 1], 1024u64),
        Some('m' | 'M') => (&text[..text.len() - 1], 1024 * 1024),
        Some('g' | 'G') => (&text[..text.len() - 1], 1024 * 1024 * 1024),
        _ => (text, 1),
    };
    let value: f64 = number
        .trim()
        .parse()
        .with_context(|| format!("invalid rate: `{text}`"))?;
    if value <= 0.0 {
        bail!("rate must be positive: `{text}`");
    }
    Ok(Some((value * factor as f64) as u64))
}

/// Formats a Unix timestamp as `YYYY-MM-DD`, without pulling in a date
/// library just for this.
fn readable_date(unix: i64) -> String {
    if unix <= 0 {
        return "—".to_string();
    }
    let total_days = unix / 86_400;
    let (mut year, mut days) = (1970, total_days);
    loop {
        let in_year = if is_leap_year(year) { 366 } else { 365 };
        if days < in_year {
            break;
        }
        days -= in_year;
        year += 1;
    }
    let months = [
        31,
        if is_leap_year(year) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 0;
    while month < 12 && days >= months[month] {
        days -= months[month];
        month += 1;
    }
    format!("{year:04}-{:02}-{:02}", month + 1, days + 1)
}

fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn truncate_text(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let cut: String = text.chars().take(limit.saturating_sub(1)).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rate_limits() {
        assert_eq!(parse_rate(None).unwrap(), None);
        assert_eq!(parse_rate(Some("1024")).unwrap(), Some(1024));
        assert_eq!(parse_rate(Some("500k")).unwrap(), Some(512_000));
        assert_eq!(parse_rate(Some("2M")).unwrap(), Some(2 * 1024 * 1024));
        assert_eq!(parse_rate(Some("1.5M")).unwrap(), Some(1_572_864));
        assert_eq!(parse_rate(Some(" 1G ")).unwrap(), Some(1 << 30));

        assert!(parse_rate(Some("fast")).is_err());
        assert!(parse_rate(Some("0")).is_err());
        assert!(parse_rate(Some("-5M")).is_err());
    }

    #[test]
    fn formats_dates() {
        assert_eq!(readable_date(0), "—");
        assert_eq!(readable_date(86_400), "1970-01-02");
        // 2024-02-29, to check the leap year.
        assert_eq!(readable_date(1_709_164_800), "2024-02-29");
        assert_eq!(readable_date(1_735_689_600), "2025-01-01");
    }

    #[test]
    fn truncates_by_characters() {
        assert_eq!(truncate_text("short", 10), "short");
        assert_eq!(truncate_text("abcdefghij", 5), "abcd…");
        // Accents count as one character, not two bytes.
        assert_eq!(truncate_text("ãéîõü", 5), "ãéîõü");
    }

    #[test]
    fn cli_accepts_the_usage_forms() {
        let a = Args::try_parse_from(["torrra", "file.torrent"]).unwrap();
        assert_eq!(a.sources, ["file.torrent"]);
        assert_eq!(a.port, 6881);
        assert!(!a.sequential);

        let a = Args::try_parse_from([
            "torrra", "-o", "/tmp", "-p", "51413", "-s", "--no-dht", "m1", "m2",
        ])
        .unwrap();
        assert_eq!(a.output, Some(PathBuf::from("/tmp")));
        assert_eq!(a.port, 51413);
        assert!(a.sequential && a.no_dht);
        assert_eq!(a.sources.len(), 2);

        // The legacy Portuguese flags keep working as hidden aliases.
        let a =
            Args::try_parse_from(["torrra", "--saida", "/tmp", "--sem-dht", "--simples"]).unwrap();
        assert_eq!(a.output, Some(PathBuf::from("/tmp")));
        assert!(a.no_dht && a.simple);

        // `--info` does not need the positional source.
        assert!(Args::try_parse_from(["torrra", "--info", "x.torrent"]).is_ok());
        // With no argument at all the interface opens empty, waiting for a magnet.
        let empty = Args::try_parse_from(["torrra"]).unwrap();
        assert!(empty.sources.is_empty());
        assert_eq!(empty.output, None);
    }

    #[test]
    fn chooses_destination_by_precedence() {
        let option = || Some(PathBuf::from("/option"));
        let pinned = || Some(PathBuf::from("/pinned"));
        let downloads = || Some(PathBuf::from("/downloads"));

        // The command-line option beats everything.
        assert_eq!(
            choose_destination(option(), pinned(), downloads()),
            PathBuf::from("/option")
        );
        // Without it, the destination.txt next to the executable.
        assert_eq!(
            choose_destination(None, pinned(), downloads()),
            PathBuf::from("/pinned")
        );
        // Without either, the Downloads folder — which is only offered when
        // the program was opened from a file manager.
        assert_eq!(
            choose_destination(None, None, downloads()),
            PathBuf::from("/downloads")
        );
        // From a terminal, with nothing configured, the usual current directory.
        assert_eq!(choose_destination(None, None, None), PathBuf::from("."));
    }

    #[test]
    fn notice_replaces_the_path_with_the_file_name() {
        let path = r"C:\Users\someone\Downloads\very\deep\broken.torrent";
        let error =
            anyhow::anyhow!("invalid bencode").context(format!("`{path}` is not a valid .torrent"));
        let notice = rejection_notice(path, &error);

        assert!(
            !notice.contains("Downloads"),
            "the long path is gone: {notice}"
        );
        assert!(
            notice.contains("broken.torrent"),
            "the name stays: {notice}"
        );
        assert!(
            notice.contains("invalid bencode"),
            "and the cause: {notice}"
        );
    }

    #[test]
    fn notice_names_the_file_when_the_error_does_not() {
        let error = anyhow::anyhow!("disk full");
        let notice = rejection_notice(r"C:\tmp\movie.torrent", &error);
        assert_eq!(notice, "movie.torrent: disk full");
    }

    #[test]
    fn hides_the_long_path_prefix() {
        assert_eq!(
            display_path(Path::new(r"\\?\C:\Users\x\Downloads")),
            r"C:\Users\x\Downloads"
        );
        assert_eq!(
            display_path(Path::new("/home/x/Downloads")),
            "/home/x/Downloads"
        );
    }
}
