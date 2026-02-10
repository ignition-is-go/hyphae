//! Hypha Cell Inspector TUI
//!
//! A real-time TUI that visualizes the hypha cell dependency graph.
//! Connects to inspector servers over TCP and streams cell snapshots.
//! Its own state is built with hypha cells, making it self-inspectable.

use std::{
    collections::{HashMap, HashSet},
    io::{self, BufRead, BufReader},
    net::TcpStream,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use clap::Parser;

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use hypha::registry::CellSnapshot;
use hypha::server::start_server;
use hypha::{Cell, CellMap, CellMutable, Gettable, MapExt, Mutable, Signal, Watchable};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph},
};
use uuid::Uuid;

/// An inspector environment to connect to.
#[derive(Clone, Debug)]
struct Environment {
    name: String,
    host: String,
    port: u16,
}

impl std::fmt::Display for Environment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} :{}", self.name, self.port)
    }
}

/// Diff frame received from the server.
#[derive(serde::Deserialize)]
struct DiffFrame {
    #[serde(rename = "type")]
    _kind: String,
    #[serde(default)]
    upsert: Vec<CellSnapshot>,
    #[serde(default)]
    remove: Vec<Uuid>,
}

/// A node in the rendered tree.
struct TreeNode {
    kind: TreeNodeKind,
    depth: usize,
    is_last_at_depth: Vec<bool>,
    has_children: bool,
    collapsed: bool,
}

enum TreeNodeKind {
    /// A real cell.
    Cell(CellSnapshot),
    /// A group header for multiple root cells sharing the same name.
    Group {
        name: String,
        count: usize,
        /// Deterministic ID for collapse tracking.
        id: Uuid,
    },
    /// A line of the expanded value for a cell.
    ValueLine {
        /// The cell this value line belongs to.
        parent_id: Uuid,
        /// The text content of this line.
        text: String,
    },
}

impl TreeNode {
    fn id(&self) -> Uuid {
        match &self.kind {
            TreeNodeKind::Cell(s) => s.id,
            TreeNodeKind::Group { id, .. } => *id,
            TreeNodeKind::ValueLine { parent_id, .. } => *parent_id,
        }
    }

    fn is_value_line(&self) -> bool {
        matches!(self.kind, TreeNodeKind::ValueLine { .. })
    }
}

/// Generate a deterministic UUID for a group name.
fn group_id(name: &str) -> Uuid {
    Uuid::new_v5(&Uuid::NAMESPACE_OID, name.as_bytes())
}

/// CLI arguments.
#[derive(Parser)]
#[command(name = "hypha-inspector", about = "Real-time TUI for inspecting hypha cell graphs")]
struct Cli {
    /// Connect to inspector servers (host:port or :port)
    #[arg(value_name = "ADDR")]
    connect: Vec<String>,

    /// Print a static snapshot and exit (non-interactive mode)
    #[arg(long)]
    dump: bool,
}

/// Parse an address string like "host:port" or ":port" into an Environment.
fn parse_addr(arg: &str) -> Option<Environment> {
    let (host, port_str) = if let Some(rest) = arg.strip_prefix(':') {
        ("127.0.0.1", rest)
    } else if let Some((h, p)) = arg.rsplit_once(':') {
        (h, p)
    } else {
        return None;
    };
    let port: u16 = port_str.parse().ok()?;
    Some(Environment {
        name: format!("{}:{}", host, port),
        host: host.to_string(),
        port,
    })
}

/// Connect to a server, read one full snapshot frame, and print cells to stdout.
fn dump_snapshot(addr: &str) -> anyhow::Result<()> {
    let env = parse_addr(addr)
        .ok_or_else(|| anyhow::anyhow!("invalid address: {} (expected host:port or :port)", addr))?;

    let stream = TcpStream::connect_timeout(
        &format!("{}:{}", env.host, env.port).parse()?,
        Duration::from_secs(5),
    )?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(stream);

    // Read the first frame (full snapshot)
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let frame: DiffFrame = serde_json::from_str(&line)?;

    let mut cells = frame.upsert;
    cells.sort_by(|a, b| a.display_name.cmp(&b.display_name));

    // Build ownership/dep maps for tree structure
    let all_dep_ids: HashSet<Uuid> = cells.iter().flat_map(|c| c.dep_ids.iter().copied()).collect();

    let roots: Vec<&CellSnapshot> = cells
        .iter()
        .filter(|c| !all_dep_ids.contains(&c.id) && c.owner_id.is_none())
        .collect();
    let owned: Vec<&CellSnapshot> = cells
        .iter()
        .filter(|c| all_dep_ids.contains(&c.id) || c.owner_id.is_some())
        .collect();

    println!("Connected to {}", addr);
    println!(
        "Cells: {}  Roots: {}  Owned: {}\n",
        cells.len(),
        roots.len(),
        owned.len()
    );

    // Print flat list sorted by display name
    let name_width = cells
        .iter()
        .map(|c| c.display_name.len())
        .max()
        .unwrap_or(20)
        .min(50);
    let caller_width = cells
        .iter()
        .filter_map(|c| c.caller.as_ref().map(|s| s.len()))
        .max()
        .unwrap_or(0)
        .min(60);

    for cell in &cells {
        let caller = cell.caller.as_deref().unwrap_or("");
        let value = cell
            .value
            .as_deref()
            .map(|v| if v.len() > 80 { &v[..80] } else { v })
            .unwrap_or("");

        println!(
            "{:<name_width$}  {:<caller_width$}  subs: {}  owned: {}  = {}",
            cell.display_name,
            caller,
            cell.subscriber_count,
            cell.owned_count,
            value,
        );
    }

    Ok(())
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if cli.dump {
        if cli.connect.is_empty() {
            anyhow::bail!("--dump requires at least one address argument");
        }
        for addr in &cli.connect {
            dump_snapshot(addr)?;
        }
        return Ok(());
    }

    // Build a tokio runtime for the inspector server
    let rt = tokio::runtime::Runtime::new()?;
    let _guard = rt.enter();

    // Start our own inspector server so we're self-inspectable
    let server = start_server("hypha-inspector");

    // --- Seed environments: self + CLI args ---
    let mut initial_envs = vec![Environment {
        name: "self".into(),
        host: "127.0.0.1".into(),
        port: server.port(),
    }];

    for arg in &cli.connect {
        if let Some(env) = parse_addr(arg) {
            initial_envs.push(env);
        } else {
            eprintln!("ignoring invalid address: {arg} (expected host:port or :port)");
        }
    }

    // Auto-select: first CLI-provided env if any, otherwise self
    let initial_selected = if initial_envs.len() > 1 { Some(1) } else { Some(0) };

    // --- Hypha cells for TUI state ---
    let environments: Cell<Vec<Environment>, CellMutable> =
        Cell::new(initial_envs).with_name("environments");
    let selected_env: Cell<Option<usize>, CellMutable> =
        Cell::new(initial_selected).with_name("selected_env");
    let snapshots: CellMap<Uuid, CellSnapshot> = CellMap::new();
    let selected_index: Cell<usize, CellMutable> = Cell::new(0).with_name("selected_index");
    let expanded: Cell<HashSet<Uuid>, CellMutable> =
        Cell::new(HashSet::new()).with_name("expanded");
    let expanded_values: Cell<HashSet<Uuid>, CellMutable> =
        Cell::new(HashSet::new()).with_name("expanded_values");

    // Derived cells
    let snapshot_entries = snapshots.entries();
    let summary = snapshot_entries
        .map(|entries| {
            let total = entries.len();
            let all_dep_ids: HashSet<Uuid> = entries
                .iter()
                .flat_map(|(_, c)| c.dep_ids.iter().copied())
                .collect();
            let roots = entries
                .iter()
                .filter(|(id, _)| !all_dep_ids.contains(id))
                .count();
            format!("Cells: {}  Roots: {}", total, roots)
        })
        .with_name("summary");

    let shutdown = Arc::new(AtomicBool::new(false));

    // --- TCP diff reader ---
    let snapshots_for_tcp = snapshots.clone();
    let selected_for_tcp = selected_env.clone();
    let envs_for_tcp = environments.clone();
    let shutdown_tcp = shutdown.clone();

    std::thread::spawn(move || {
        let mut current_stream: Option<BufReader<TcpStream>> = None;
        let mut current_port: Option<u16> = None;
        let mut known_ids: HashSet<Uuid> = HashSet::new();

        loop {
            if shutdown_tcp.load(Ordering::SeqCst) {
                break;
            }

            let selected = selected_for_tcp.get();
            let envs = envs_for_tcp.get();

            let target_port = selected.and_then(|idx| envs.get(idx).map(|e| e.port));

            if target_port != current_port {
                current_stream = None;
                current_port = None;

                // Clear map on environment switch
                for id in known_ids.drain() {
                    snapshots_for_tcp.remove(&id);
                }

                if let Some(port) = target_port {
                    let host = selected
                        .and_then(|idx| envs.get(idx).map(|e| e.host.clone()))
                        .unwrap_or_else(|| "127.0.0.1".into());

                    match TcpStream::connect_timeout(
                        &format!("{}:{}", host, port).parse().unwrap(),
                        Duration::from_secs(2),
                    ) {
                        Ok(stream) => {
                            stream
                                .set_read_timeout(Some(Duration::from_millis(300)))
                                .ok();
                            current_stream = Some(BufReader::new(stream));
                            current_port = Some(port);
                        }
                        Err(_) => {
                            std::thread::sleep(Duration::from_millis(500));
                            continue;
                        }
                    }
                }
            }

            if let Some(ref mut reader) = current_stream {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => {
                        current_stream = None;
                        current_port = None;
                    }
                    Ok(_) => {
                        if let Ok(frame) = serde_json::from_str::<DiffFrame>(&line) {
                            for cell in frame.upsert {
                                known_ids.insert(cell.id);
                                snapshots_for_tcp.insert(cell.id, cell);
                            }
                            for id in frame.remove {
                                known_ids.remove(&id);
                                snapshots_for_tcp.remove(&id);
                            }
                        }
                    }
                    Err(ref e)
                        if e.kind() == io::ErrorKind::TimedOut
                            || e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(_) => {
                        current_stream = None;
                        current_port = None;
                    }
                }
            } else {
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    });

    // --- Event channel: unifies cell changes + keyboard input ---
    enum UiEvent {
        Key(crossterm::event::KeyEvent),
        CellChanged,
    }

    let (ui_tx, ui_rx) = flume::unbounded::<UiEvent>();

    // Subscribe to all state cells — any change triggers a re-render
    let _guards: Vec<hypha::SubscriptionGuard> = {
        let tx = ui_tx.clone();
        vec![
            snapshot_entries.subscribe({
                let tx = tx.clone();
                move |_: &Signal<_>| { let _ = tx.try_send(UiEvent::CellChanged); }
            }),
            environments.subscribe({
                let tx = tx.clone();
                move |_: &Signal<_>| { let _ = tx.try_send(UiEvent::CellChanged); }
            }),
            selected_env.subscribe({
                let tx = tx.clone();
                move |_: &Signal<_>| { let _ = tx.try_send(UiEvent::CellChanged); }
            }),
            selected_index.subscribe({
                let tx = tx.clone();
                move |_: &Signal<_>| { let _ = tx.try_send(UiEvent::CellChanged); }
            }),
            expanded.subscribe({
                let tx = tx.clone();
                move |_: &Signal<_>| { let _ = tx.try_send(UiEvent::CellChanged); }
            }),
            expanded_values.subscribe({
                let tx = tx.clone();
                move |_: &Signal<_>| { let _ = tx.try_send(UiEvent::CellChanged); }
            }),
            summary.subscribe({
                let tx = tx.clone();
                move |_: &Signal<_>| { let _ = tx.try_send(UiEvent::CellChanged); }
            }),
        ]
    };

    // Keyboard input thread
    let shutdown_input = shutdown.clone();
    let input_tx = ui_tx.clone();
    std::thread::spawn(move || {
        while !shutdown_input.load(Ordering::SeqCst) {
            if event::poll(Duration::from_millis(50)).unwrap_or(false) {
                if let Ok(Event::Key(key)) = event::read() {
                    if input_tx.send(UiEvent::Key(key)).is_err() {
                        break;
                    }
                }
            }
        }
    });

    // --- TUI setup ---
    enable_raw_mode()?;
    crossterm::execute!(io::stdout(), EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    // Initial render
    let mut needs_render = true;

    // --- Main event loop ---
    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }

        if needs_render {
            let envs = environments.get();
            let sel = selected_env.get();
            let cells: Vec<CellSnapshot> =
                snapshot_entries.get().into_iter().map(|(_, v)| v).collect();
            let summary_text = summary.get();
            let expanded_set = expanded.get();
            let expanded_vals = expanded_values.get();
            let tree_nodes = build_tree(&cells, &expanded_set, &expanded_vals);
            let tree_len = tree_nodes.len();
            let selected = selected_index.get().min(tree_len.saturating_sub(1));

            terminal.draw(|frame| {
                let area = frame.area();
                let chunks = Layout::vertical([
                    Constraint::Length(3),
                    Constraint::Min(0),
                ])
                .split(area);

                render_header(frame, chunks[0], &envs, sel, &summary_text);
                render_tree(frame, chunks[1], &tree_nodes, selected);
            })?;

            needs_render = false;
        }

        // Block until something happens
        let Ok(ev) = ui_rx.recv_timeout(Duration::from_secs(1)) else {
            continue;
        };

        // Drain any queued events to coalesce rapid updates
        let mut key_events = Vec::new();
        match ev {
            UiEvent::Key(key) => key_events.push(key),
            UiEvent::CellChanged => needs_render = true,
        }
        while let Ok(ev) = ui_rx.try_recv() {
            match ev {
                UiEvent::Key(key) => key_events.push(key),
                UiEvent::CellChanged => needs_render = true,
            }
        }

        // Process key events
        for key in key_events {
            if key.kind != KeyEventKind::Press {
                continue;
            }

            // Read current state for navigation
            let cells: Vec<CellSnapshot> =
                snapshot_entries.get().into_iter().map(|(_, v)| v).collect();
            let expanded_set = expanded.get();
            let expanded_vals = expanded_values.get();
            let tree_nodes = build_tree(&cells, &expanded_set, &expanded_vals);
            let tree_len = tree_nodes.len();
            let selected = selected_index.get().min(tree_len.saturating_sub(1));

            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => {
                    shutdown.store(true, Ordering::SeqCst);
                    break;
                }
                KeyCode::Tab => {
                    let envs = environments.get();
                    if !envs.is_empty() {
                        let current = selected_env.get().unwrap_or(0);
                        let next = (current + 1) % envs.len();
                        selected_env.set(Some(next));
                        selected_index.set(0);
                    }
                }
                KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
                    let idx = (c as usize) - ('1' as usize);
                    let envs = environments.get();
                    if idx < envs.len() {
                        selected_env.set(Some(idx));
                        selected_index.set(0);
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if selected + 1 < tree_len {
                        selected_index.set(selected + 1);
                    }
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    selected_index.set(selected.saturating_sub(1));
                }
                KeyCode::Left | KeyCode::Char('h') => {
                    if let Some(node) = tree_nodes.get(selected) {
                        if node.has_children && !node.collapsed && !node.is_value_line() {
                            // Collapse this node
                            let mut set = expanded.get();
                            set.remove(&node.id());
                            expanded.set(set);
                        } else if node.depth > 0 {
                            // Navigate to parent: scan backwards for the nearest
                            // node at a shallower depth
                            for i in (0..selected).rev() {
                                if tree_nodes[i].depth < node.depth {
                                    selected_index.set(i);
                                    break;
                                }
                            }
                        }
                    }
                }
                KeyCode::Right | KeyCode::Char('l') => {
                    if let Some(node) = tree_nodes.get(selected) {
                        if node.has_children && node.collapsed {
                            let mut set = expanded.get();
                            set.insert(node.id());
                            expanded.set(set);
                        } else if node.has_children && selected + 1 < tree_len {
                            selected_index.set(selected + 1);
                        }
                    }
                }
                KeyCode::Enter => {
                    if let Some(node) = tree_nodes.get(selected) {
                        match &node.kind {
                            TreeNodeKind::Cell(snapshot) if snapshot.value.is_some() => {
                                let mut set = expanded_values.get();
                                if set.contains(&snapshot.id) {
                                    set.remove(&snapshot.id);
                                } else {
                                    set.insert(snapshot.id);
                                }
                                expanded_values.set(set);
                            }
                            TreeNodeKind::ValueLine { parent_id, .. } => {
                                // Toggle off when pressing Enter on a value line
                                let mut set = expanded_values.get();
                                set.remove(parent_id);
                                expanded_values.set(set);
                            }
                            _ => {}
                        }
                    }
                }
                KeyCode::Char('-') => {
                    // Collapse one level: un-expand all nodes at the deepest visible depth
                    let mut max_depth = 0usize;
                    for node in &tree_nodes {
                        if node.has_children && !node.collapsed && !node.is_value_line() {
                            max_depth = max_depth.max(node.depth);
                        }
                    }
                    let mut set = expanded.get();
                    let mut changed = false;
                    for node in &tree_nodes {
                        if node.has_children
                            && !node.collapsed
                            && !node.is_value_line()
                            && node.depth == max_depth
                        {
                            set.remove(&node.id());
                            changed = true;
                        }
                    }
                    if changed {
                        expanded.set(set);
                    }
                }
                KeyCode::Char('=') | KeyCode::Char('+') => {
                    // Expand one level: expand all collapsed nodes at the shallowest depth
                    let mut min_depth = usize::MAX;
                    for node in &tree_nodes {
                        if node.collapsed && !node.is_value_line() {
                            min_depth = min_depth.min(node.depth);
                        }
                    }
                    if min_depth < usize::MAX {
                        let mut set = expanded.get();
                        for node in &tree_nodes {
                            if node.collapsed
                                && !node.is_value_line()
                                && node.depth == min_depth
                            {
                                set.insert(node.id());
                            }
                        }
                        expanded.set(set);
                    }
                }
                KeyCode::PageDown => {
                    selected_index.set((selected + 20).min(tree_len.saturating_sub(1)));
                }
                KeyCode::PageUp => {
                    selected_index.set(selected.saturating_sub(20));
                }
                KeyCode::Home => {
                    selected_index.set(0);
                }
                KeyCode::End => {
                    selected_index.set(tree_len.saturating_sub(1));
                }
                _ => {}
            }
        }
    }

    // Cleanup
    drop(_guards);
    disable_raw_mode()?;
    crossterm::execute!(io::stdout(), LeaveAlternateScreen)?;
    drop(server);

    Ok(())
}

fn render_header(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    envs: &[Environment],
    selected: Option<usize>,
    summary: &str,
) {
    let chunks =
        Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)]).split(area);

    let env_spans: Vec<Span> = if envs.is_empty() {
        vec![Span::styled(
            " No environments ",
            Style::default().fg(Color::DarkGray),
        )]
    } else {
        envs.iter()
            .enumerate()
            .flat_map(|(i, env)| {
                let style = if Some(i) == selected {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                };
                let sep = if i > 0 {
                    vec![Span::raw("  ")]
                } else {
                    vec![]
                };
                let mut spans = sep;
                spans.push(Span::styled(format!("[{}] {}", i + 1, env), style));
                spans
            })
            .collect()
    };

    let envs_paragraph = Paragraph::new(Line::from(env_spans)).block(
        Block::default()
            .title(" Hypha Inspector ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)),
    );
    frame.render_widget(envs_paragraph, chunks[0]);

    let summary_line = Line::from(vec![
        Span::styled(summary, Style::default().fg(Color::Green)),
        Span::raw("  "),
        Span::styled(
            "q:quit  j/k:nav  h/l:fold  -/+:level  Enter:value  Tab:env",
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    let summary_widget = Paragraph::new(summary_line).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)),
    );
    frame.render_widget(summary_widget, chunks[1]);
}

fn render_tree(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    tree_nodes: &[TreeNode],
    selected: usize,
) {
    if tree_nodes.is_empty() {
        let empty = Paragraph::new(Span::styled(
            "  No cells. Connect to an environment to inspect.",
            Style::default().fg(Color::DarkGray),
        ))
        .block(Block::default().borders(Borders::ALL));
        frame.render_widget(empty, area);
        return;
    }

    let visible_height = area.height.saturating_sub(2) as usize;
    let selected = selected.min(tree_nodes.len().saturating_sub(1));

    // Auto-scroll viewport to keep selection visible
    let scroll = if selected < visible_height / 2 {
        0
    } else if selected + visible_height / 2 >= tree_nodes.len() {
        tree_nodes.len().saturating_sub(visible_height)
    } else {
        selected.saturating_sub(visible_height / 2)
    };

    let items: Vec<ListItem> = tree_nodes
        .iter()
        .enumerate()
        .skip(scroll)
        .take(visible_height)
        .map(|(idx, node)| {
            let mut prefix = String::new();
            for (i, &is_last) in node.is_last_at_depth.iter().enumerate() {
                if i == node.depth {
                    break;
                }
                if is_last {
                    prefix.push_str("   ");
                } else {
                    prefix.push_str("│  ");
                }
            }
            if node.depth > 0 {
                let is_last = node
                    .is_last_at_depth
                    .get(node.depth - 1)
                    .copied()
                    .unwrap_or(false);
                if is_last {
                    prefix.push_str("└─ ");
                } else {
                    prefix.push_str("├─ ");
                }
            }

            let is_selected = idx == selected;

            let fold_indicator = if node.has_children {
                if node.collapsed { "▶ " } else { "▼ " }
            } else {
                "  "
            };

            let spans = match &node.kind {
                TreeNodeKind::Group { name, count, .. } => {
                    let name_style = if is_selected {
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                            .fg(Color::Magenta)
                            .add_modifier(Modifier::BOLD)
                    };

                    vec![
                        Span::raw(prefix),
                        Span::styled(fold_indicator, Style::default().fg(Color::DarkGray)),
                        Span::styled(name.clone(), name_style),
                        Span::styled(
                            format!(" ({}x)", count),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ]
                }
                TreeNodeKind::Cell(snapshot) => {
                    let name_style = if is_selected {
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    } else if node.depth == 0 {
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::White)
                    };

                    let id_short = snapshot.id.to_string()[..8].to_string();
                    let stats = format!(
                        "subs: {}  owned: {}",
                        snapshot.subscriber_count, snapshot.owned_count
                    );

                    let mut spans = vec![
                        Span::raw(prefix),
                        Span::styled(fold_indicator, Style::default().fg(Color::DarkGray)),
                        Span::styled(snapshot.display_name.clone(), name_style),
                        Span::styled(
                            format!(" ({})", id_short),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ];

                    if let Some(ref caller) = snapshot.caller {
                        spans.push(Span::raw(" "));
                        spans.push(Span::styled(
                            caller.clone(),
                            Style::default().fg(Color::Blue),
                        ));
                    }

                    spans.extend([
                        Span::raw("  "),
                        Span::styled(stats, Style::default().fg(Color::Cyan)),
                    ]);

                    if let Some(ref value) = snapshot.value {
                        spans.push(Span::raw("  "));
                        spans.push(Span::styled(
                            format!("= {}", truncate_inline(value, 80)),
                            Style::default().fg(Color::Magenta),
                        ));
                    }

                    spans
                }
                TreeNodeKind::ValueLine { text, .. } => {
                    let style = if is_selected {
                        Style::default().fg(Color::Black).bg(Color::Cyan)
                    } else {
                        Style::default().fg(Color::Magenta)
                    };

                    vec![
                        Span::raw(prefix),
                        Span::raw("  "),
                        Span::styled(text.clone(), style),
                    ]
                }
            };

            ListItem::new(Line::from(spans))
        })
        .collect();

    let tree_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(format!(
            " Cell Tree ({}/{}) ",
            selected + 1,
            tree_nodes.len()
        ));

    let list = List::new(items).block(tree_block);
    frame.render_widget(list, area);
}

/// Build a flattened tree from the cell snapshots.
/// Children are determined by `dep_ids` (cells this cell owns guards for).
/// Roots are cells not listed in any other cell's `dep_ids`.
/// Named roots are shown first. Duplicate-named roots are grouped under a fold header.
fn build_tree(
    cells: &[CellSnapshot],
    expanded_set: &HashSet<Uuid>,
    expanded_values: &HashSet<Uuid>,
) -> Vec<TreeNode> {
    let by_id: HashMap<Uuid, &CellSnapshot> = cells.iter().map(|c| (c.id, c)).collect();

    // Build children map from dep_ids (parent → deps it owns guards for).
    // This correctly handles 1:N cases where multiple cells subscribe to the same source.
    let mut children: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    for cell in cells {
        for dep_id in &cell.dep_ids {
            if by_id.contains_key(dep_id) {
                children.entry(cell.id).or_default().push(*dep_id);
            }
        }
    }

    // A cell is a root if no other cell lists it as a dependency.
    let all_dep_ids: HashSet<Uuid> = cells.iter().flat_map(|c| c.dep_ids.iter().copied()).collect();
    let roots: Vec<&CellSnapshot> = cells
        .iter()
        .filter(|c| !all_dep_ids.contains(&c.id))
        .collect();

    // Partition roots: named vs unnamed
    let mut named: Vec<&CellSnapshot> = Vec::new();
    let mut unnamed: Vec<&CellSnapshot> = Vec::new();
    for root in &roots {
        if root.name.is_some() {
            named.push(root);
        } else {
            unnamed.push(root);
        }
    }

    // Group named roots by display_name
    let mut name_groups: HashMap<&str, Vec<&CellSnapshot>> = HashMap::new();
    for root in &named {
        name_groups
            .entry(&root.display_name)
            .or_default()
            .push(root);
    }

    // Collect group names, sorted alphabetically
    let mut group_names: Vec<&str> = name_groups.keys().copied().collect();
    group_names.sort();

    // Sort unnamed by display_name
    unnamed.sort_by(|a, b| a.display_name.cmp(&b.display_name));

    // Total top-level items: groups/singles from named + unnamed cells
    let top_level_count = group_names.len() + unnamed.len();

    let mut result = Vec::new();
    let mut top_idx = 0;
    let mut visited = HashSet::new();

    fn walk(
        id: Uuid,
        by_id: &HashMap<Uuid, &CellSnapshot>,
        children: &HashMap<Uuid, Vec<Uuid>>,
        expanded_set: &HashSet<Uuid>,
        expanded_values: &HashSet<Uuid>,
        visited: &mut HashSet<Uuid>,
        depth: usize,
        is_last_at_depth: &mut Vec<bool>,
        result: &mut Vec<TreeNode>,
    ) {
        let Some(snapshot) = by_id.get(&id) else {
            return;
        };

        // Prevent cycles in the dependency graph
        if !visited.insert(id) {
            return;
        }

        let has_children = children.get(&id).is_some_and(|kids| !kids.is_empty());
        let is_collapsed = !expanded_set.contains(&id);
        let is_expanded = expanded_values.contains(&id);

        result.push(TreeNode {
            kind: TreeNodeKind::Cell((*snapshot).clone()),
            depth,
            is_last_at_depth: is_last_at_depth.clone(),
            has_children,
            collapsed: is_collapsed,
        });

        // Emit value lines if this cell's value is expanded
        if is_expanded {
            if let Some(ref value) = snapshot.value {
                let lines = format_value(value);
                for line in lines {
                    result.push(TreeNode {
                        kind: TreeNodeKind::ValueLine {
                            parent_id: id,
                            text: line,
                        },
                        depth: depth + 1,
                        is_last_at_depth: is_last_at_depth.clone(),
                        has_children: false,
                        collapsed: false,
                    });
                }
            }
        }

        if !is_collapsed {
            if let Some(kids) = children.get(&id) {
                let mut sorted_kids = kids.clone();
                sorted_kids.sort_by(|a, b| {
                    let a_name = by_id.get(a).map(|c| &c.display_name);
                    let b_name = by_id.get(b).map(|c| &c.display_name);
                    a_name.cmp(&b_name)
                });

                for (i, &child_id) in sorted_kids.iter().enumerate() {
                    let is_last = i == sorted_kids.len() - 1;
                    is_last_at_depth.push(is_last);
                    walk(
                        child_id,
                        by_id,
                        children,
                        expanded_set,
                        expanded_values,
                        visited,
                        depth + 1,
                        is_last_at_depth,
                        result,
                    );
                    is_last_at_depth.pop();
                }
            }
        }
    }

    // Emit named roots (grouped if duplicates, standalone if unique)
    for name in &group_names {
        let group = &name_groups[name];
        let is_last_top = top_idx == top_level_count - 1;
        top_idx += 1;

        if group.len() == 1 {
            // Single named root — emit directly at depth 0
            let mut is_last_at_depth = vec![is_last_top];
            walk(
                group[0].id,
                &by_id,
                &children,
                expanded_set,
                expanded_values,
                &mut visited,
                0,
                &mut is_last_at_depth,
                &mut result,
            );
        } else {
            // Multiple roots with same name — emit group header
            let gid = group_id(name);
            let is_collapsed = !expanded_set.contains(&gid);

            result.push(TreeNode {
                kind: TreeNodeKind::Group {
                    name: name.to_string(),
                    count: group.len(),
                    id: gid,
                },
                depth: 0,
                is_last_at_depth: vec![is_last_top],
                has_children: true,
                collapsed: is_collapsed,
            });

            if !is_collapsed {
                let mut sorted_group: Vec<&CellSnapshot> = group.clone();
                sorted_group.sort_by_key(|c| c.id);

                for (i, cell) in sorted_group.iter().enumerate() {
                    let is_last = i == sorted_group.len() - 1;
                    let mut is_last_at_depth = vec![is_last_top, is_last];
                    walk(
                        cell.id,
                        &by_id,
                        &children,
                        expanded_set,
                        expanded_values,
                        &mut visited,
                        1,
                        &mut is_last_at_depth,
                        &mut result,
                    );
                }
            }
        }
    }

    // Emit unnamed roots
    for root in &unnamed {
        let is_last_top = top_idx == top_level_count - 1;
        top_idx += 1;
        let mut is_last_at_depth = vec![is_last_top];
        walk(
            root.id,
            &by_id,
            &children,
            expanded_set,
            expanded_values,
            &mut visited,
            0,
            &mut is_last_at_depth,
            &mut result,
        );
    }

    result
}

/// Format a debug value string for expanded display.
/// Tries to parse as JSON for pretty-printing, falls back to indented debug formatting.
fn format_value(raw: &str) -> Vec<String> {
    // Try JSON parse first (some values may serialize as JSON-compatible)
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(raw) {
        if let Ok(pretty) = serde_json::to_string_pretty(&json) {
            return pretty.lines().map(String::from).collect();
        }
    }

    // Fall back to splitting the debug string into readable lines with indentation
    format_debug_string(raw)
}

/// Format a Rust Debug string into indented lines.
/// Handles `{ }`, `[ ]`, `( )` nesting, and `,` as line separators.
fn format_debug_string(s: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut indent: usize = 0;
    let indent_str = "  ";
    let mut chars = s.chars().peekable();
    let mut in_string = false;

    while let Some(c) = chars.next() {
        if in_string {
            current.push(c);
            if c == '"' {
                in_string = false;
            } else if c == '\\' {
                // Skip escaped character
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            continue;
        }

        match c {
            '"' => {
                current.push(c);
                in_string = true;
            }
            '{' | '[' | '(' => {
                current.push(c);
                // Check if the matching close is very soon (compact notation)
                let rest: String = chars.clone().take(30).collect();
                let close = match c {
                    '{' => '}',
                    '[' => ']',
                    _ => ')',
                };
                // If close bracket appears within a short span with no nested openers, keep inline
                if let Some(pos) = rest.find(close) {
                    let inner = &rest[..pos];
                    if pos < 20 && !inner.contains(['{', '[', '(']) {
                        for _ in 0..=pos {
                            if let Some(ch) = chars.next() {
                                current.push(ch);
                            }
                        }
                        continue;
                    }
                }
                let trimmed = current.trim_start().to_string();
                if !trimmed.is_empty() {
                    lines.push(format!("{}{}", indent_str.repeat(indent), trimmed));
                    current.clear();
                }
                indent += 1;
            }
            '}' | ']' | ')' => {
                let trimmed = current.trim_start().to_string();
                if !trimmed.is_empty() {
                    lines.push(format!("{}{}", indent_str.repeat(indent), trimmed));
                    current.clear();
                }
                indent = indent.saturating_sub(1);
                current.push(c);
            }
            ',' => {
                current.push(c);
                let trimmed = current.trim_start().to_string();
                if !trimmed.is_empty() {
                    lines.push(format!("{}{}", indent_str.repeat(indent), trimmed));
                }
                current.clear();
            }
            _ => {
                current.push(c);
            }
        }
    }

    let trimmed = current.trim().to_string();
    if !trimmed.is_empty() {
        lines.push(format!("{}{}", indent_str.repeat(indent), trimmed));
    }

    // If the result is just one short line, return it directly
    if lines.len() == 1 {
        return lines;
    }

    lines
}

/// Truncate a string for inline display, adding ellipsis if needed.
fn truncate_inline(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        let mut truncated = s[..max_len].to_string();
        truncated.push('…');
        truncated
    }
}
