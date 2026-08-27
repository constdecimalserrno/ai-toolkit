use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Gauge, Paragraph, Row, Table};
use ratatui::Frame;
use sysinfo::{Disks, Pid, ProcessesToUpdate, System};

/// Substrings that mark a process as AI-ish (matched against name + exe + argv).
const AI_HINTS: &[&str] = &[
    "ollama", "llama", "claude", "chatgpt", "openai", "anthropic", "copilot", "cursor",
    "lm studio", "lmstudio", "mlx", "whisper", "stable-diffusion", "comfyui", "vllm",
    "koboldcpp", "gemini", "perplexity", "codeium", "tabnine", "huggingface", "torch",
    "tensorflow", "onnx", "diffusion", "langchain", "aider", "gpt4", "gpt-4", "llm",
];

type NetRates = HashMap<u32, (f64, f64)>; // pid -> (bytes_in/s, bytes_out/s)
type DirUsage = Vec<(String, u64)>; // project -> bytes, biggest first
type Sessions = Vec<Session>;

struct Session {
    age: i64, // seconds since the transcript last grew
    tokens: u64,
    cwd: String,
    label: String, // first thing the user asked for
}

/// Width of the bouncing-ball track, in cells.
const TRACK: usize = 8;
/// Milliseconds per animation frame (~2.5 fps).
const FRAME_MS: u128 = 400;

/// Set once at startup: did `caffeinate` actually start?
static AWAKE: OnceLock<bool> = OnceLock::new();

fn main() -> std::io::Result<()> {
    caffeinate();

    let net: Arc<Mutex<NetRates>> = Arc::default();
    {
        let net = net.clone();
        std::thread::spawn(move || net_loop(&net));
    }

    let dirs: Arc<Mutex<DirUsage>> = Arc::default();
    {
        let dirs = dirs.clone();
        std::thread::spawn(move || du_loop(&dirs));
    }

    let sessions: Arc<Mutex<Sessions>> = Arc::default();
    {
        let sessions = sessions.clone();
        std::thread::spawn(move || session_loop(&sessions));
    }

    let mut sys = System::new_all();
    let mut disks = Disks::new_with_refreshed_list();
    let mut last = Instant::now() - Duration::from_secs(1);
    let start = Instant::now();

    ratatui::run(|terminal| loop {
        if last.elapsed() >= Duration::from_millis(1000) {
            sys.refresh_cpu_all();
            sys.refresh_memory();
            sys.refresh_processes(ProcessesToUpdate::All, true);
            disks.refresh(true);
            last = Instant::now();
        }
        let rates = net.lock().unwrap().clone();
        let dirs = dirs.lock().unwrap().clone();
        let sessions = sessions.lock().unwrap();
        let tick = (start.elapsed().as_millis() / FRAME_MS) as usize;
        terminal.draw(|f| draw(f, &sys, &disks, &rates, &dirs, &sessions, tick))?;
        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(k) = event::read()? {
                if k.kind == KeyEventKind::Press
                    && matches!(k.code, KeyCode::Char('q') | KeyCode::Esc)
                {
                    break Ok(());
                }
            }
        }
    })
}

/// macOS: hold the machine (and display) awake for as long as we run. `-w <pid>` makes
/// caffeinate exit with us, so there is nothing to clean up.
fn caffeinate() {
    let ok = Command::new("caffeinate")
        .args(["-dimsu", "-w", &std::process::id().to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_ok();
    let _ = AWAKE.set(ok);
}

// ---------- network sampling (macOS `nettop`) ----------

fn net_loop(shared: &Mutex<NetRates>) {
    let mut prev: HashMap<u32, (u64, u64)> = HashMap::new();
    let mut last = Instant::now();
    loop {
        let sample = nettop_sample();
        let dt = last.elapsed().as_secs_f64().max(0.001);
        if !prev.is_empty() && !sample.is_empty() {
            let rates = sample
                .iter()
                .filter_map(|(pid, (bi, bo))| {
                    let (pi, po) = prev.get(pid)?;
                    Some((
                        *pid,
                        (
                            bi.saturating_sub(*pi) as f64 / dt,
                            bo.saturating_sub(*po) as f64 / dt,
                        ),
                    ))
                })
                .collect();
            *shared.lock().unwrap() = rates;
        }
        if !sample.is_empty() {
            prev = sample;
            last = Instant::now();
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

fn nettop_sample() -> HashMap<u32, (u64, u64)> {
    let out = Command::new("nettop")
        .args(["-P", "-L", "1", "-x", "-J", "bytes_in,bytes_out"])
        .output();
    match out {
        Ok(o) => parse_nettop(&String::from_utf8_lossy(&o.stdout)),
        Err(_) => HashMap::new(),
    }
}

/// Rows look like `Google Chrome H.1804,34156099,1198683,`
fn parse_nettop(s: &str) -> HashMap<u32, (u64, u64)> {
    s.lines()
        .filter_map(|l| {
            let mut f = l.split(',');
            let pid = f.next()?.rsplit('.').next()?.parse().ok()?;
            let (bi, bo) = (f.next()?.trim().parse().ok()?, f.next()?.trim().parse().ok()?);
            Some((pid, (bi, bo)))
        })
        .collect()
}

// ---------- claude sessions (transcripts under ~/.claude/projects) ----------

/// Transcripts are multi-MB JSONL; python already ships on this box (the statusline
/// runs on it) so let it do the parsing rather than growing a JSON dep here.
const SESSION_SCAN: &str = r"
import json, glob, os, time
now = time.time()
for f in glob.glob(os.path.expanduser('~/.claude/projects/*/*.jsonl')):
    try:
        age = now - os.path.getmtime(f)
        size = os.path.getsize(f)
    except OSError:
        continue
    if age > 1800:
        continue
    with open(f, errors='replace') as fh:
        label = ''
        for _ in range(40):                      # the ask is near the top of the file
            line = fh.readline()
            if not line:
                break
            try:
                d = json.loads(line)
            except ValueError:
                continue
            if d.get('type') != 'user':
                continue
            c = (d.get('message') or {}).get('content')
            t = c if isinstance(c, str) else ' '.join(
                x.get('text', '') for x in c or [] if isinstance(x, dict))
            t = ' '.join(t.split())
            if t and not t.startswith('<'):      # skip slash-command wrappers
                label = t[:150]
                break
        fh.seek(max(0, size - 65536))            # context size lives in the last reply
        for line in reversed(fh.read().splitlines()[1:]):
            try:
                d = json.loads(line)
            except ValueError:
                continue
            u = (d.get('message') or {}).get('usage')
            if not u:
                continue
            u = (u.get('iterations') or [u])[-1]     # last pass = context as it stands
            ctx = sum(u.get(k, 0) or 0 for k in
                      ('input_tokens', 'cache_creation_input_tokens', 'cache_read_input_tokens'))
            print('\t'.join(str(x) for x in (int(age), ctx, d.get('cwd', ''), label)))
            break
";

fn session_loop(shared: &Mutex<Sessions>) {
    loop {
        if let Ok(o) = Command::new("python3").args(["-c", SESSION_SCAN]).output() {
            *shared.lock().unwrap() = parse_sessions(&String::from_utf8_lossy(&o.stdout));
        }
        std::thread::sleep(Duration::from_secs(15));
    }
}

/// One row per session: `<age secs>\t<context tokens>\t<cwd>\t<what they asked for>`
fn parse_sessions(out: &str) -> Sessions {
    let mut v: Sessions = out
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            if f.len() < 4 {
                return None;
            }
            Some(Session {
                age: f[0].parse().ok()?,
                tokens: f[1].parse().ok()?,
                cwd: f[2].to_string(),
                label: f[3].to_string(),
            })
        })
        .collect();
    v.sort_by_key(|s| s.age);
    v
}

// ---------- project disk usage (`du`) ----------

/// Only this tree is scanned, and only whole projects inside it.
/// Override with `MONITOR_PROJECT_ROOT` (absolute path).
const PROJECT_ROOT: &str = "Documents";

fn project_root() -> String {
    std::env::var("MONITOR_PROJECT_ROOT").unwrap_or_else(|_| {
        format!(
            "{}/{PROJECT_ROOT}",
            std::env::var("HOME").unwrap_or_else(|_| "/".into())
        )
    })
}
/// Telltale signs of a project root.
const MARKERS: [&str; 4] = [".git", "context", "docs", "CLAUDE.md"];

// ponytail: re-walk + `du -sk` every 30 min, no incremental watching. A cold scan of
// ~26 projects (~500G, Rust target dirs) takes ~2.5 min, so don't shorten this without
// swapping in fsevents or skipping build dirs.
fn du_loop(shared: &Mutex<DirUsage>) {
    let root = project_root();
    loop {
        let mut projects = Vec::new();
        find_projects(Path::new(&root), 0, &mut projects);
        let mut v = du_sizes(&projects, &root);
        v.sort_by_key(|(_, b)| std::cmp::Reverse(*b));
        if !v.is_empty() {
            *shared.lock().unwrap() = v;
        }
        std::thread::sleep(Duration::from_secs(1800));
    }
}

/// Descending stops at the first match, so a repo's own `docs/` or a nested `.git`
/// never splits off as its own project. Plain container folders are walked through.
fn find_projects(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > 3 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        if !path.is_dir() || e.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        if MARKERS.iter().any(|m| path.join(m).exists()) {
            out.push(path);
        } else {
            find_projects(&path, depth + 1, out);
        }
    }
}

fn du_sizes(projects: &[PathBuf], root: &str) -> DirUsage {
    if projects.is_empty() {
        return DirUsage::new();
    }
    let out = Command::new("du")
        .args(["-s", "-k"])
        .args(projects.iter().map(|p| p.as_os_str()))
        .output();
    match out {
        Ok(o) => parse_du(&String::from_utf8_lossy(&o.stdout), root),
        Err(_) => DirUsage::new(),
    }
}

/// Rows look like `12345<TAB>/Users/me/Documents/clockwork` in KB blocks;
/// paths are labelled relative to `root`.
fn parse_du(s: &str, root: &str) -> DirUsage {
    let root = format!("{}/", root.trim_end_matches('/'));
    s.lines()
        .filter_map(|l| {
            let (kb, path) = l.split_once('\t')?;
            let label = path.strip_prefix(&root).unwrap_or(path);
            Some((label.to_string(), kb.trim().parse::<u64>().ok()? * 1024))
        })
        .collect()
}

// ---------- rendering ----------

fn draw(
    f: &mut Frame,
    sys: &System,
    disks: &Disks,
    rates: &NetRates,
    dirs: &DirUsage,
    sessions: &Sessions,
    tick: usize,
) {
    let [body, bottom, footer] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(10), Constraint::Length(1)])
            .areas(f.area());
    let [left, right] = Layout::horizontal([Constraint::Percentage(50); 2]).areas(body);
    let cores = sys.cpus().len() as u16;
    let [cpu, mem, memtop] = Layout::vertical([
        Constraint::Length(cores + 4),
        Constraint::Length(5),
        Constraint::Min(4),
    ])
    .areas(left);
    let [sess, net] = Layout::vertical([Constraint::Percentage(50); 2]).areas(right);
    let [storage, hogs, claude] =
        Layout::horizontal([Constraint::Ratio(1, 3); 3]).areas(bottom);
    let [hint, clock] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(21)]).areas(footer);

    draw_cpu(f, cpu, sys);
    draw_mem(f, mem, sys);
    draw_memtop(f, memtop, sys);
    draw_storage(f, storage, disks);
    draw_hogs(f, hogs, dirs);
    draw_sessions(f, sess, sessions);
    draw_net(f, net, sys, rates);
    draw_claude(f, claude);

    let awake = if *AWAKE.get().unwrap_or(&false) {
        format!(" · ☕ awake {}", dur(tick as u64 * FRAME_MS as u64 / 1000))
    } else {
        String::new()
    };
    f.render_widget(
        Line::from(format!(
            " q/esc quit · cpu+mem 1s · network 2s (nettop) · du 30m{awake} "
        ))
        .dark_gray(),
        hint,
    );
    f.render_widget(
        Line::from(format!("{} {} ", bounce(tick), now_local()))
            .cyan()
            .right_aligned(),
        clock,
    );
}

/// Claude Code plan usage, as cached by the statusline hook.
fn draw_claude(f: &mut Frame, area: Rect) {
    let u = claude_usage();
    let age = u.as_ref().map_or(0, |u| epoch() - u.ts);
    let title = if age > 300 {
        format!(" CLAUDE  (stale {}m) ", age / 60)
    } else {
        " CLAUDE ".to_string()
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let Some(u) = u else {
        let cached = usage_path().is_some_and(|p| Path::new(&p).exists());
        let why = if cached {
            "usage row unreadable \u{2014} statusline format changed?"
        } else {
            "no usage cached \u{2014} needs a live Claude session"
        };
        f.render_widget(Paragraph::new(why.dark_gray()), inner);
        return;
    };

    let [five, seven, _, pace] = Layout::vertical([Constraint::Length(1); 4]).areas(inner);
    f.render_widget(
        gauge(u.five.0, &format!("5h  {:.0}%   \u{21ba}{}", u.five.0, u.five.1)),
        five,
    );
    f.render_widget(
        gauge(u.seven.0, &format!("7d  {:.0}%   \u{21ba}{}", u.seven.0, u.seven.1)),
        seven,
    );

    // Ahead of the even-burn line is slack, behind it is a bonfire.
    let (mood, color) = match u.pace {
        p if p >= 5 => ("\u{1f634} coasting", Color::Green),
        p if p > -5 => ("\u{1f44c} on pace", Color::Green),
        p if p > -15 => ("\u{1f525} burning fast", Color::Yellow),
        _ => ("\u{1f525}\u{1f525} torching it", Color::Red),
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw("pace "),
            Span::styled(format!("{:+}%", u.pace), Style::default().fg(color)),
            Span::raw(format!("  {mood}")),
        ])),
        pace,
    );
}

fn draw_cpu(f: &mut Frame, area: Rect, sys: &System) {
    let block = Block::default().borders(Borders::ALL).title(" CPU ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let [total, per_core] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);

    let g = sys.global_cpu_usage();
    f.render_widget(gauge(g, &format!("{g:5.1}% total")), total);

    let lines: Vec<Line> = sys
        .cpus()
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let u = c.cpu_usage();
            Line::from(vec![
                Span::raw(format!("{i:>3} ")),
                Span::styled(bar(u, 20), Style::default().fg(load_color(u))),
                Span::raw(format!(" {u:5.1}%")),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines), per_core);
}

fn draw_mem(f: &mut Frame, area: Rect, sys: &System) {
    let block = Block::default().borders(Borders::ALL).title(" MEMORY ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let [ram, swap] = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(inner);

    let used = pct(sys.used_memory(), sys.total_memory());
    f.render_widget(
        gauge(
            used,
            &format!(
                "RAM  {} / {}",
                bytes(sys.used_memory()),
                bytes(sys.total_memory())
            ),
        ),
        ram,
    );
    let spct = pct(sys.used_swap(), sys.total_swap());
    f.render_widget(
        gauge(
            spct,
            &format!(
                "SWAP {} / {}",
                bytes(sys.used_swap()),
                bytes(sys.total_swap())
            ),
        ),
        swap,
    );
}

// ponytail: per-process RSS summed by name, so shared pages are double counted. Good
// enough to answer "who is eating the RAM"; use `footprint`/vmmap for exact numbers.
fn draw_memtop(f: &mut Frame, area: Rect, sys: &System) {
    let total = sys.total_memory();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" MEMORY BY PROCESS  (of {}) ", bytes(total)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut by_name: HashMap<String, u64> = HashMap::new();
    for p in sys.processes().values() {
        *by_name.entry(name_of(p)).or_default() += p.memory();
    }
    let mut top: Vec<_> = by_name.into_iter().collect();
    top.sort_by_key(|(_, m)| std::cmp::Reverse(*m));

    let lines: Vec<Line> = top
        .iter()
        .take(inner.height as usize)
        .map(|(n, m)| {
            let p = pct(*m, total);
            Line::from(vec![
                Span::raw(format!("{n:<20.20} ")),
                Span::styled(bar(p * 3.0, 12), Style::default().fg(load_color(p * 3.0))),
                Span::raw(format!(" {:>8} {p:4.1}%", bytes(*m))),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_hogs(f: &mut Frame, area: Rect, dirs: &DirUsage) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" DISK BY PROJECT  ({}) ", project_root()));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let Some((_, biggest)) = dirs.first() else {
        f.render_widget(Paragraph::new("scanning\u{2026}".dark_gray()), inner);
        return;
    };
    let lines: Vec<Line> = dirs
        .iter()
        .take(inner.height as usize)
        .map(|(n, b)| {
            Line::from(vec![
                Span::raw(format!("{n:<20.20} ")),
                Span::styled(bar(pct(*b, *biggest), 12), Style::default().fg(Color::Blue)),
                Span::raw(format!(" {:>8}", bytes(*b))),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_storage(f: &mut Frame, area: Rect, disks: &Disks) {
    let block = Block::default().borders(Borders::ALL).title(" STORAGE ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut seen = Vec::new();
    let mut y = inner.y;
    for d in disks.list() {
        if d.total_space() == 0 || seen.contains(&d.mount_point().to_owned()) || y >= inner.bottom()
        {
            continue;
        }
        seen.push(d.mount_point().to_owned());
        let used = d.total_space() - d.available_space();
        let label = format!(
            "{:<22} {} / {}",
            d.mount_point().display().to_string().chars().take(22).collect::<String>(),
            bytes(used),
            bytes(d.total_space())
        );
        f.render_widget(
            gauge(pct(used, d.total_space()), &label),
            Rect::new(inner.x, y, inner.width, 1),
        );
        y += 1;
    }
}

fn draw_sessions(f: &mut Frame, area: Rect, sessions: &Sessions) {
    let rows = sessions.iter().map(|s| {
        Row::new(vec![
            Cell::from(s.cwd.rsplit('/').next().unwrap_or("?").to_string()).magenta(),
            Cell::from(ktok(s.tokens)),
            Cell::from(dur(s.age as u64)),
            Cell::from(s.label.clone()),
        ])
    });
    let title = format!(" CLAUDE SESSIONS  {} active (30m) ", sessions.len());
    f.render_widget(
        Table::new(
            rows,
            [
                Constraint::Length(16),
                Constraint::Length(6),
                Constraint::Length(5),
                Constraint::Min(10),
            ],
        )
        .header(
            Row::new(["WHERE", "CTX", "AGE", "WORKING ON"])
                .style(Style::default().add_modifier(Modifier::BOLD).fg(Color::Cyan)),
        )
        .block(Block::default().borders(Borders::ALL).title(title)),
        area,
    );
}

fn draw_net(f: &mut Frame, area: Rect, sys: &System, rates: &NetRates) {
    let mut procs: Vec<_> = sys
        .processes()
        .values()
        .map(|p| (p, rate_of(rates, p.pid())))
        .filter(|(_, (i, o))| i + o > 512.0)
        .collect();
    procs.sort_by(|a, b| (b.1 .0 + b.1 .1).total_cmp(&(a.1 .0 + a.1 .1)));

    let (ti, to) = rates.values().fold((0.0, 0.0), |a, r| (a.0 + r.0, a.1 + r.1));
    let title = format!(" NETWORK  ↓{}  ↑{} ", rate(ti), rate(to));

    let rows = procs.iter().map(|(p, (i, o))| {
        let name = Cell::from(name_of(p));
        Row::new(vec![
            if is_ai(p) { name.magenta() } else { name },
            Cell::from(p.pid().to_string()),
            Cell::from(format!("{:.1}", p.cpu_usage())),
            Cell::from(bytes(p.memory())),
            Cell::from(rate(*i)),
            Cell::from(rate(*o)),
        ])
    });
    f.render_widget(proc_table(rows, &title), area);
}

fn proc_table<'a>(rows: impl IntoIterator<Item = Row<'a>>, title: &'a str) -> Table<'a> {
    Table::new(
        rows,
        [
            Constraint::Min(10),
            Constraint::Length(7),
            Constraint::Length(6),
            Constraint::Length(9),
            Constraint::Length(11),
            Constraint::Length(11),
        ],
    )
    .header(
        Row::new(["PROCESS", "PID", "CPU%", "MEM", "NET IN", "NET OUT"])
            .style(Style::default().add_modifier(Modifier::BOLD).fg(Color::Cyan)),
    )
    .block(Block::default().borders(Borders::ALL).title(title.to_string()))
}

// ---------- helpers ----------

fn is_ai(p: &sysinfo::Process) -> bool {
    let name = p.name().to_string_lossy().to_lowercase();
    let mut hay = name.clone();
    if let Some(e) = p.exe() {
        hay.push_str(&e.to_string_lossy().to_lowercase());
    }
    // argv only for interpreters/launchers, else every shell run from an AI dir matches
    if ["python", "node", "bun", "deno", "uv", "ruby", "java", "electron"]
        .iter()
        .any(|i| name.contains(i))
    {
        for a in p.cmd().iter().take(6) {
            hay.push_str(&a.to_string_lossy().to_lowercase());
        }
    }
    AI_HINTS.iter().any(|h| hay.contains(h))
}

fn name_of(p: &sysinfo::Process) -> String {
    p.name().to_string_lossy().chars().take(24).collect()
}

fn rate_of(rates: &NetRates, pid: Pid) -> (f64, f64) {
    rates.get(&(pid.as_u32())).copied().unwrap_or((0.0, 0.0))
}

fn gauge<'a>(pct: f32, label: &str) -> Gauge<'a> {
    Gauge::default()
        .gauge_style(Style::default().fg(load_color(pct)))
        .ratio((pct as f64 / 100.0).clamp(0.0, 1.0))
        .label(label.to_string())
}

fn load_color(pct: f32) -> Color {
    match pct {
        p if p < 60.0 => Color::Green,
        p if p < 85.0 => Color::Yellow,
        _ => Color::Red,
    }
}

fn bar(pct: f32, width: usize) -> String {
    let filled = ((pct / 100.0 * width as f32).round() as usize).min(width);
    format!("{}{}", "█".repeat(filled), "·".repeat(width - filled))
}

fn pct(used: u64, total: u64) -> f32 {
    if total == 0 {
        0.0
    } else {
        used as f32 / total as f32 * 100.0
    }
}

/// Ball bouncing across a `TRACK`-wide track, one cell per frame.
fn bounce(tick: usize) -> String {
    let span = TRACK * 2 - 2;
    let i = tick % span;
    let pos = if i < TRACK { i } else { span - i };
    format!("[{}●{}]", " ".repeat(pos), " ".repeat(TRACK - 1 - pos))
}

#[derive(Clone)]
struct Usage {
    ts: i64,
    five: (f32, String),  // (used %, resets in)
    seven: (f32, String),
    pace: i32,
}

/// The statusline rewrites the row on its own schedule and occasionally emits one with
/// fields missing, so poll every 10s and keep the last good row until a new one parses.
fn claude_usage() -> Option<Usage> {
    static LAST: Mutex<(i64, Option<Usage>)> = Mutex::new((0, None));
    let now = epoch();
    let mut last = LAST.lock().unwrap();
    if now - last.0 >= 10 {
        last.0 = now;
        if let Some(u) = read_usage() {
            last.1 = Some(u);
        }
    }
    last.1.clone()
}

fn read_usage() -> Option<Usage> {
    parse_usage(&std::fs::read_to_string(usage_path()?).ok()?)
}

fn usage_path() -> Option<String> {
    Some(format!(
        "{}/.claude/statusline/usage.txt",
        std::env::var("HOME").ok()?
    ))
}

/// One row written by the statusline hook:
/// `<epoch>\t<5h %>\t<5h reset>\t<7d %>\t<7d reset>\t<pace>`
fn parse_usage(raw: &str) -> Option<Usage> {
    let f: Vec<&str> = raw.trim_end().split('\t').collect();
    if f.len() < 6 {
        return None;
    }
    Some(Usage {
        ts: f[0].parse().ok()?,
        five: (f[1].parse().ok()?, f[2].to_string()),
        seven: (f[3].parse().ok()?, f[4].to_string()),
        pace: f[5].parse().ok()?,
    })
}

fn epoch() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn now_local() -> String {
    clock_str(epoch() + tz_offset())
}

fn clock_str(secs: i64) -> String {
    let s = secs.rem_euclid(86_400);
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

/// Local UTC offset, read once at startup (a DST flip mid-session is not tracked).
fn tz_offset() -> i64 {
    static OFF: OnceLock<i64> = OnceLock::new();
    *OFF.get_or_init(|| {
        Command::new("date")
            .arg("+%z")
            .output()
            .ok()
            .and_then(|o| parse_tz(String::from_utf8_lossy(&o.stdout).trim()))
            .unwrap_or(0)
    })
}

/// `-0500` -> -18000
fn parse_tz(s: &str) -> Option<i64> {
    let (sign, hm) = s.split_at(s.len().checked_sub(4)?);
    let secs = hm.get(..2)?.parse::<i64>().ok()? * 3600 + hm.get(2..4)?.parse::<i64>().ok()? * 60;
    Some(if sign.starts_with('-') { -secs } else { secs })
}

/// 149032 -> `149k`
fn ktok(n: u64) -> String {
    if n >= 1000 {
        format!("{}k", n / 1000)
    } else {
        n.to_string()
    }
}

/// `93s` / `12m` / `1h23m` / `2d4h`
fn dur(secs: u64) -> String {
    match (secs / 86400, secs / 3600 % 24, secs / 60 % 60) {
        (0, 0, 0) => format!("{secs}s"),
        (0, 0, m) => format!("{m}m"),
        (0, h, m) => format!("{h}h{m:02}m"),
        (d, h, _) => format!("{d}d{h}h"),
    }
}

fn bytes(b: u64) -> String {
    const U: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{b}B") } else { format!("{v:.1}{}", U[i]) }
}

fn rate(bps: f64) -> String {
    if bps < 1.0 {
        "-".into()
    } else {
        format!("{}/s", bytes(bps as u64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nettop_and_formats() {
        let s = ",bytes_in,bytes_out,\nGoogle Chrome H.1804,34156099,1198683,\nbogus line\n";
        let m = parse_nettop(s);
        assert_eq!(m.len(), 1);
        assert_eq!(m[&1804], (34156099, 1198683));
        assert_eq!(bytes(2048), "2.0K");
        assert_eq!(rate(0.4), "-");
        assert_eq!(bar(50.0, 4), "██··");
        assert_eq!(pct(1, 4), 25.0);
    }

    #[test]
    fn parses_du_and_clock() {
        let d = parse_du("48\t/home/me/Tickets/Babylon\n4096\t/home/me/clockwork\nbogus\n", "/home/me");
        assert_eq!(
            d,
            vec![
                ("Tickets/Babylon".to_string(), 48 * 1024),
                ("clockwork".to_string(), 4096 * 1024),
            ]
        );
        assert_eq!(parse_tz("-0530"), Some(-19_800));
        assert_eq!(parse_tz("+0200"), Some(7_200));
        assert_eq!(parse_tz("nope"), None);
        assert_eq!(clock_str(0), "00:00:00");
        assert_eq!(clock_str(-1), "23:59:59");
        assert_eq!(clock_str(86_399 + 86_400), "23:59:59");
    }

    #[test]
    fn ball_bounces() {
        assert_eq!(bounce(0), "[●       ]");
        assert_eq!(bounce(7), "[       ●]");
        assert_eq!(bounce(13), "[ ●      ]");
        assert_eq!(bounce(14), bounce(0));
        assert!((0..40).all(|t| bounce(t).chars().count() == TRACK + 2));
    }

    #[test]
    fn finds_projects_but_not_their_insides() {
        let root = std::env::temp_dir().join("monitor-proj-test");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("Tickets/Babylon/docs")).unwrap();
        std::fs::create_dir_all(root.join("Tickets/Babylon/context")).unwrap();
        std::fs::create_dir_all(root.join("Notes/system-tui/.git")).unwrap();
        std::fs::create_dir_all(root.join("Notes/empty-container/sub")).unwrap();

        let mut found = Vec::new();
        find_projects(&root, 0, &mut found);
        let mut labels: Vec<String> = found
            .iter()
            .map(|p| p.strip_prefix(&root).unwrap().display().to_string())
            .collect();
        labels.sort();
        assert_eq!(labels, ["Notes/system-tui", "Tickets/Babylon"]);
        let u = parse_usage("100\t27\t1h31m\t18\t6d5h\t-7\n").unwrap();
        assert_eq!((u.ts, u.five.0, u.seven.1.as_str(), u.pace), (100, 27.0, "6d5h", -7));
        assert!(parse_usage("100\t27\t1h31m\n").is_none());
        assert!(parse_usage("100\t\t\t\t\t\n").is_none());
        assert_eq!(
            [dur(9), dur(600), dur(4980), dur(180_000)],
            ["9s", "10m", "1h23m", "2d2h"]
        );
        assert_eq!([ktok(999), ktok(149_032)], ["999", "149k"]);
        let s = parse_sessions("90\t149032\t/a/system-tui\tadd a clock\nshort\n");
        assert_eq!(s.len(), 1);
        assert_eq!(
            (s[0].tokens, s[0].cwd.as_str(), s[0].label.as_str()),
            (149_032, "/a/system-tui", "add a clock")
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}

