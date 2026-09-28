use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseButton, MouseEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
    Terminal,
};
use std::{
    collections::HashSet,
    io,
    process::Command,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

// Debian Red color theme (#d70a53)
const DEBIAN_RED: Color = Color::Rgb(215, 10, 83);

#[derive(Clone, Debug, PartialEq)]
enum PackageSource {
    Apt,
    Flatpak,
}

#[derive(Clone, Debug, PartialEq, Copy)]
enum SourceFilter {
    All,
    Apt,
    Flatpak,
}

impl SourceFilter {
    fn next(&self) -> Self {
        match self {
            Self::All => Self::Apt,
            Self::Apt => Self::Flatpak,
            Self::Flatpak => Self::All,
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Self::All => "All (APT + Flatpak)",
            Self::Apt => "APT Only",
            Self::Flatpak => "Flatpak Only",
        }
    }
}

#[derive(Clone, Debug)]
struct Package {
    display_name: String,
    name: String,
    description: String,
    installed: bool,
    source: PackageSource,
    relevance_score: i32,
}

#[derive(PartialEq, Clone, Copy)]
enum AppScreen {
    MainMenu,
    SearchInstall,
    InstalledPackages,
}

#[derive(PartialEq)]
enum ActivePanel {
    SearchInput,
    PackageList,
    InstalledSearchInput,
    InstalledList,
}

struct App {
    current_screen: AppScreen,
    menu_state: ListState,

    // Search screen state
    search_query: String,
    search_packages: Vec<Package>,
    search_list_state: ListState,
    search_source_filter: SourceFilter,
    active_panel: ActivePanel,
    is_searching: bool,
    spinner_frame: usize,
    last_spinner_update: Instant,

    // Debouncing & Instant Search
    last_keystroke: Option<Instant>,
    last_searched_query: String,
    last_searched_filter: SourceFilter,

    // Installed screen state
    installed_packages: Vec<Package>,
    filtered_installed_packages: Vec<Package>,
    installed_query: String,
    installed_list_state: ListState,
    installed_source_filter: SourceFilter,

    // Confirmation dialog state
    pending_remove: Option<Package>,
}

impl App {
    fn new() -> Self {
        let mut menu_state = ListState::default();
        menu_state.select(Some(0));

        let mut search_list_state = ListState::default();
        search_list_state.select(Some(0));

        let mut installed_list_state = ListState::default();
        installed_list_state.select(Some(0));

        Self {
            current_screen: AppScreen::MainMenu,
            menu_state,
            search_query: String::new(),
            search_packages: Vec::new(),
            search_list_state,
            search_source_filter: SourceFilter::All,
            active_panel: ActivePanel::SearchInput,
            is_searching: false,
            spinner_frame: 0,
            last_spinner_update: Instant::now(),
            last_keystroke: None,
            last_searched_query: String::new(),
            last_searched_filter: SourceFilter::All,
            installed_packages: Vec::new(),
            filtered_installed_packages: Vec::new(),
            installed_query: String::new(),
            installed_list_state,
            installed_source_filter: SourceFilter::All,
            pending_remove: None,
        }
    }

    fn next_menu_item(&mut self) {
        let i = match self.menu_state.selected() {
            Some(i) => if i >= 3 { 0 } else { i + 1 },
            None => 0,
        };
        self.menu_state.select(Some(i));
    }

    fn prev_menu_item(&mut self) {
        let i = match self.menu_state.selected() {
            Some(i) => if i == 0 { 3 } else { i - 1 },
            None => 0,
        };
        self.menu_state.select(Some(i));
    }

    fn filter_installed(&mut self) {
        let q = self.installed_query.to_lowercase();
        self.filtered_installed_packages = self.installed_packages
            .iter()
            .filter(|p| {
                let matches_source = match self.installed_source_filter {
                    SourceFilter::All => true,
                    SourceFilter::Apt => p.source == PackageSource::Apt,
                    SourceFilter::Flatpak => p.source == PackageSource::Flatpak,
                };

                if !matches_source {
                    return false;
                }

                if q.trim().is_empty() {
                    true
                } else {
                    p.display_name.to_lowercase().contains(&q)
                        || p.name.to_lowercase().contains(&q)
                        || p.description.to_lowercase().contains(&q)
                }
            })
            .cloned()
            .collect();

        if self.filtered_installed_packages.is_empty() {
            self.installed_list_state.select(None);
        } else {
            self.installed_list_state.select(Some(0));
        }
    }
}

// Helper to center popups
fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

// Calculates search relevance score
fn calculate_relevance(title: &str, desc: &str, query: &str) -> i32 {
    let q = query.to_lowercase();
    let t = title.to_lowercase();
    let d = desc.to_lowercase();

    if t == q {
        1000
    } else if t.starts_with(&q) {
        500 + (100 - t.len().min(100) as i32)
    } else if t.contains(&q) {
        200
    } else if d.contains(&q) {
        50
    } else {
        0
    }
}

// Quick batch check for installed APT packages
async fn get_installed_apt_packages() -> HashSet<String> {
    let mut set = HashSet::new();
    if let Ok(out) = tokio::process::Command::new("dpkg-query")
        .args(["-W", "-f=${Package}\n"])
        .output()
        .await
    {
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            let pkg = line.trim();
            if !pkg.is_empty() {
                set.insert(pkg.to_string());
            }
        }
    }
    set
}

// Quick batch check for installed Flatpak app IDs
async fn get_installed_flatpak_apps() -> HashSet<String> {
    let mut set = HashSet::new();
    if let Ok(out) = tokio::process::Command::new("flatpak")
        .args(["list", "--columns=application"])
        .output()
        .await
    {
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            let id = line.trim();
            if !id.is_empty() && id != "Application ID" && id != "Application" {
                set.insert(id.to_string());
            }
        }
    }
    set
}

// Searches APT repository via apt-cache (Optimized Batch Status Check)
async fn fetch_apt_packages(query: &str) -> Vec<Package> {
    let installed_set = get_installed_apt_packages().await;

    let output = tokio::process::Command::new("apt-cache")
        .args(["search", query])
        .output()
        .await;

    let mut results = Vec::new();

    if let Ok(out) = output {
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            if let Some((name, desc)) = line.split_once(" - ") {
                let pkg_name = name.trim().to_string();
                let pkg_desc = desc.trim().to_string();
                let score = calculate_relevance(&pkg_name, &pkg_desc, query);
                let installed = installed_set.contains(&pkg_name);

                results.push(Package {
                    display_name: pkg_name.clone(),
                    name: pkg_name,
                    description: pkg_desc,
                    installed,
                    source: PackageSource::Apt,
                    relevance_score: score,
                });
            }
        }
    }
    results
}

// Searches Flatpak applications (Optimized Batch Status Check)
async fn fetch_flatpak_packages(query: &str) -> Vec<Package> {
    let installed_set = get_installed_flatpak_apps().await;

    let output = tokio::process::Command::new("flatpak")
        .args(["search", query, "--columns=name:f,application:f,description:f"])
        .output()
        .await;

    let mut results = Vec::new();

    if let Ok(out) = output {
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            let parts: Vec<&str> = line.split('\t').collect();

            if parts.len() >= 2 {
                let display_name = parts[0].trim().to_string();
                let app_id = parts[1].trim().to_string();
                let app_desc = if parts.len() >= 3 { parts[2].trim().to_string() } else { "No description".to_string() };

                if app_id.is_empty() || display_name.contains("Name") {
                    continue;
                }

                let score = calculate_relevance(&display_name, &app_desc, query);
                let installed = installed_set.contains(&app_id);

                results.push(Package {
                    display_name,
                    name: app_id,
                    description: app_desc,
                    installed,
                    source: PackageSource::Flatpak,
                    relevance_score: score,
                });
            }
        }
    }
    results
}

// Unified search with Source Filter
async fn fetch_packages(query: String, filter: SourceFilter) -> Vec<Package> {
    if query.trim().is_empty() { return vec![]; }

    let mut all_results = Vec::new();

    match filter {
        SourceFilter::All => {
            let (apt_res, flatpak_res) = tokio::join!(
                fetch_apt_packages(&query),
                fetch_flatpak_packages(&query)
            );
            all_results.extend(apt_res);
            all_results.extend(flatpak_res);
        }
        SourceFilter::Apt => {
            all_results.extend(fetch_apt_packages(&query).await);
        }
        SourceFilter::Flatpak => {
            all_results.extend(fetch_flatpak_packages(&query).await);
        }
    }

    all_results.sort_by(|a, b| b.relevance_score.cmp(&a.relevance_score));
    all_results.truncate(50);
    all_results
}

// Fetch all installed packages (APT + ALL Flatpaks)
fn load_installed_packages() -> Vec<Package> {
    let mut results = Vec::new();

    // 1. APT Installed
    if let Ok(out) = Command::new("dpkg-query").args(["-W", "-f=${Package}\t${binary:Summary}\n"]).output() {
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            let parts: Vec<&str> = line.split('\t').collect();
            if !parts.is_empty() {
                let name = parts[0].trim().to_string();
                let desc = if parts.len() > 1 && !parts[1].trim().is_empty() {
                    parts[1].trim().to_string()
                } else {
                    "Installed APT package".to_string()
                };
                if !name.is_empty() {
                    results.push(Package {
                        display_name: name.clone(),
                        name,
                        description: desc,
                        installed: true,
                        source: PackageSource::Apt,
                        relevance_score: 0,
                    });
                }
            }
        }
    }

    // 2. Flatpak Installed
    if let Ok(out) = Command::new("flatpak").args(["list", "--columns=name,application,description"]).output() {
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            let parts: Vec<&str> = if line.contains('\t') {
                line.split('\t').collect()
            } else {
                line.split("  ").filter(|s| !s.trim().is_empty()).collect()
            };

            if !parts.is_empty() {
                let display_name = parts[0].trim().to_string();
                let app_id = if parts.len() > 1 { parts[1].trim().to_string() } else { display_name.clone() };
                let desc = if parts.len() > 2 && !parts[2].trim().is_empty() {
                    parts[2].trim().to_string()
                } else {
                    "Installed Flatpak package/runtime".to_string()
                };

                if display_name == "Name" || app_id == "Application ID" || app_id == "Application" {
                    continue;
                }

                if !display_name.is_empty() {
                    results.push(Package {
                        display_name,
                        name: app_id,
                        description: desc,
                        installed: true,
                        source: PackageSource::Flatpak,
                        relevance_score: 0,
                    });
                }
            }
        }
    }

    results.sort_by(|a, b| a.display_name.to_lowercase().cmp(&b.display_name.to_lowercase()));
    results
}

// Install package action
fn install_package(pkg: &Package, terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> io::Result<bool> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;

    println!("\n=== Elderapt Package Installer ===");
    println!("Source: {:?}", pkg.source);
    println!("Installing package: {} ({}) ...\n", pkg.display_name, pkg.name);

    let status = match pkg.source {
        PackageSource::Apt => Command::new("sudo").args(["apt-get", "install", "-y", &pkg.name]).status()?,
        PackageSource::Flatpak => Command::new("flatpak").args(["install", "-y", &pkg.name]).status()?,
    };

    println!("\nPress [ENTER] to return to Elderapt...");
    let mut buf = String::new();
    let _ = io::stdin().read_line(&mut buf);

    enable_raw_mode()?;
    execute!(terminal.backend_mut(), EnterAlternateScreen, EnableMouseCapture)?;
    terminal.clear()?;

    Ok(status.success())
}

// Remove package action
fn remove_package(pkg: &Package, terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> io::Result<bool> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;

    println!("\n=== Elderapt Package Remover ===");
    println!("Source: {:?}", pkg.source);
    println!("Removing package: {} ({}) ...\n", pkg.display_name, pkg.name);

    let status = match pkg.source {
        PackageSource::Apt => Command::new("sudo").args(["apt-get", "remove", "-y", &pkg.name]).status()?,
        PackageSource::Flatpak => Command::new("flatpak").args(["uninstall", "-y", &pkg.name]).status()?,
    };

    println!("\nPress [ENTER] to return to Elderapt...");
    let mut buf = String::new();
    let _ = io::stdin().read_line(&mut buf);

    enable_raw_mode()?;
    execute!(terminal.backend_mut(), EnterAlternateScreen, EnableMouseCapture)?;
    terminal.clear()?;

    Ok(status.success())
}

// System Upgrade action
fn run_system_upgrade(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;

    println!("\n=== Elderapt System Upgrade ===");
    println!("Updating APT repositories and upgrading packages...\n");
    let _ = Command::new("sudo").args(["apt-get", "update"]).status();
    let _ = Command::new("sudo").args(["apt-get", "upgrade", "-y"]).status();

    println!("\nUpdating Flatpak applications...\n");
    let _ = Command::new("flatpak").args(["update", "-y"]).status();

    println!("\nSystem upgrade completed! Press [ENTER] to return to Elderapt...");
    let mut buf = String::new();
    let _ = io::stdin().read_line(&mut buf);

    enable_raw_mode()?;
    execute!(terminal.backend_mut(), EnterAlternateScreen, EnableMouseCapture)?;
    terminal.clear()?;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new();

    // --- PROCESSAMENTO DE ARGUMENTOS CLI ---
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 {
        match args[1].as_str() {
            "install" | "search" => {
                app.current_screen = AppScreen::SearchInstall;
                app.active_panel = ActivePanel::SearchInput;

                // Se o utilizador passou um nome de pacote (ex: `elderapt install firefox`)
                if args.len() > 2 {
                    app.search_query = args[2..].join(" ");
                    app.last_keystroke = Some(Instant::now());
                    app.is_searching = true;
                }
            }
            "list" => {
                app.installed_packages = load_installed_packages();
                app.installed_query.clear();
                app.active_panel = ActivePanel::InstalledSearchInput;
                app.current_screen = AppScreen::InstalledPackages;

                // Se o utilizador passou um filtro (ex: `elderapt list pipewire`)
                if args.len() > 2 {
                    app.installed_query = args[2..].join(" ");
                }

                app.filter_installed();
            }
            _ => {} // Qualquer outro argumento desconhecido abre o menu principal normalmente
        }
    }

    // Channel returns (query, packages) to verify match
    let (tx, mut rx) = mpsc::channel::<(String, Vec<Package>)>(1);

    loop {
        // Receive completed async search results
        if let Ok((res_query, new_packages)) = rx.try_recv() {
            if res_query == app.search_query {
                app.search_packages = new_packages;
                app.is_searching = false;
                if app.search_packages.is_empty() {
                    app.search_list_state.select(None);
                } else {
                    app.search_list_state.select(Some(0));
                }
            }
        }

        // Debounce Trigger (250 ms)
        if let Some(last_time) = app.last_keystroke {
            if last_time.elapsed() >= Duration::from_millis(250) {
                app.last_keystroke = None;
                let query = app.search_query.trim().to_string();
                let filter = app.search_source_filter;

                if query.is_empty() {
                    app.search_packages.clear();
                    app.is_searching = false;
                    app.search_list_state.select(None);
                    app.last_searched_query.clear();
                } else if query != app.last_searched_query || filter != app.last_searched_filter {
                    app.last_searched_query = query.clone();
                    app.last_searched_filter = filter;
                    app.is_searching = true;

                    let tx_clone = tx.clone();
                    tokio::spawn(async move {
                        let pkgs = fetch_packages(query.clone(), filter).await;
                        let _ = tx_clone.send((query, pkgs)).await;
                    });
                }
            }
        }

        if app.is_searching {
            if app.last_spinner_update.elapsed() >= Duration::from_millis(120) {
                app.spinner_frame = (app.spinner_frame + 1) % 4;
                app.last_spinner_update = Instant::now();
            }
        }

        terminal.draw(|f| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Min(10),
                    Constraint::Length(3),
                ])
                .split(f.size());

            let spinner_chars = ['|', '/', '-', '\\'];

            match app.current_screen {
                AppScreen::MainMenu => {
                    let title = Paragraph::new("")
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .border_style(Style::default().fg(DEBIAN_RED))
                                .title(Line::from(" Elderapt Package Manager - Main Menu ").style(Style::default().fg(Color::White)))
                        );
                    f.render_widget(title, chunks[0]);

                    let menu_items = vec![
                        ListItem::new(" 1. Search, Install & Remove Packages"),
                        ListItem::new(" 2. View Installed Packages"),
                        ListItem::new(" 3. System Upgrade (APT & Flatpak)"),
                        ListItem::new(" 4. Exit"),
                    ];

                    let menu_list = List::new(menu_items)
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .border_style(Style::default().fg(DEBIAN_RED))
                                .title(Line::from(" Select an Option ").style(Style::default().fg(Color::White)))
                        )
                        .highlight_style(
                            Style::default()
                                .bg(DEBIAN_RED)
                                .fg(Color::White)
                                .add_modifier(Modifier::BOLD),
                        )
                        .highlight_symbol("> ");

                    f.render_stateful_widget(menu_list, chunks[1], &mut app.menu_state);

                    let footer = Paragraph::new("")
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .border_style(Style::default().fg(DEBIAN_RED))
                                .title(Line::from(" [↑/↓ / Scroll / Click] Select | [Enter] Confirm | [q/Esc] Exit ").style(Style::default().fg(Color::White)))
                        );
                    f.render_widget(footer, chunks[2]);
                }

                AppScreen::SearchInstall => {
                    let is_search_active = app.active_panel == ActivePanel::SearchInput;
                    let is_list_active = app.active_panel == ActivePanel::PackageList;

                    let search_title_text = if app.is_searching {
                        format!(
                            " Search Packages [Source: {}] (Searching {}...) ",
                            app.search_source_filter.label(),
                            spinner_chars[app.spinner_frame]
                        )
                    } else {
                        format!(" Search Packages [Source: {}] ", app.search_source_filter.label())
                    };

                    let search_box = Paragraph::new(app.search_query.as_str())
                        .style(Style::default().fg(Color::White))
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .border_style(Style::default().fg(if is_search_active { DEBIAN_RED } else { Color::DarkGray }))
                                .title(Line::from(search_title_text).style(Style::default().fg(if is_search_active { Color::White } else { Color::DarkGray })))
                        );
                    f.render_widget(search_box, chunks[0]);

                    let main_chunks = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                        .split(chunks[1]);

                    let items: Vec<ListItem> = app
                        .search_packages
                        .iter()
                        .map(|pkg| {
                            let source_tag = match pkg.source {
                                PackageSource::Apt => "[APT]",
                                PackageSource::Flatpak => "[Flatpak]",
                            };

                            let status_tag = if pkg.installed { " [Installed]" } else { "" };
                            let content = format!("{} {}{}", pkg.display_name, source_tag, status_tag);

                            if pkg.installed {
                                ListItem::new(content).style(Style::default().fg(Color::Green))
                            } else {
                                ListItem::new(content).style(Style::default().fg(Color::White))
                            }
                        })
                        .collect();

                    let list = List::new(items)
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .border_style(Style::default().fg(if is_list_active { DEBIAN_RED } else { Color::DarkGray }))
                                .title(Line::from(" Results ").style(Style::default().fg(if is_list_active { Color::White } else { Color::DarkGray })))
                        )
                        .highlight_style(
                            Style::default()
                                .bg(DEBIAN_RED)
                                .fg(Color::White)
                                .add_modifier(Modifier::BOLD),
                        )
                        .highlight_symbol("> ");

                    f.render_stateful_widget(list, main_chunks[0], &mut app.search_list_state);

                    let selected_desc = match app.search_list_state.selected() {
                        Some(i) if !app.search_packages.is_empty() => {
                            let pkg = &app.search_packages[i];
                            format!(
                                "Name: {}\nSystem ID: {}\nSource: {:?}\nStatus: {}\nRelevance: {}\n\nDescription:\n{}",
                                pkg.display_name,
                                pkg.name,
                                pkg.source,
                                if pkg.installed { "Installed on system" } else { "Not installed" },
                                pkg.relevance_score,
                                pkg.description
                            )
                        }
                        _ => "No package selected.".to_string(),
                    };

                    let detail_box = Paragraph::new(selected_desc)
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .border_style(Style::default().fg(DEBIAN_RED))
                                .title(Line::from(" Details ").style(Style::default().fg(Color::White)))
                        );
                    f.render_widget(detail_box, main_chunks[1]);

                    let footer_text = match app.active_panel {
                        ActivePanel::SearchInput => " [Type] Auto Search | [↓] Focus List | [F3] Toggle Source | [Tab] Switch Panel | [Esc] Main Menu ",
                        ActivePanel::PackageList => " [↑/↓] Navigate | [Enter] Install | [r] Remove | [s/F3] Toggle Source | [Tab] Switch Panel | [Esc] Main Menu ",
                        _ => " [Esc] Main Menu ",
                    };
                    let footer = Paragraph::new("")
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .border_style(Style::default().fg(DEBIAN_RED))
                                .title(Line::from(footer_text).style(Style::default().fg(Color::White)))
                        );
                    f.render_widget(footer, chunks[2]);
                }

                AppScreen::InstalledPackages => {
                    let is_search_active = app.active_panel == ActivePanel::InstalledSearchInput;
                    let is_list_active = app.active_panel == ActivePanel::InstalledList;

                    let title_text = format!(
                        " Filter Installed Packages [Source: {}] ",
                        app.installed_source_filter.label()
                    );

                    let search_box = Paragraph::new(app.installed_query.as_str())
                        .style(Style::default().fg(Color::White))
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .border_style(Style::default().fg(if is_search_active { DEBIAN_RED } else { Color::DarkGray }))
                                .title(Line::from(title_text).style(Style::default().fg(if is_search_active { Color::White } else { Color::DarkGray })))
                        );
                    f.render_widget(search_box, chunks[0]);

                    let main_chunks = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                        .split(chunks[1]);

                    let items: Vec<ListItem> = app
                        .filtered_installed_packages
                        .iter()
                        .map(|pkg| {
                            let source_tag = match pkg.source {
                                PackageSource::Apt => "[APT]",
                                PackageSource::Flatpak => "[Flatpak]",
                            };
                            let content = format!("{} {}", pkg.display_name, source_tag);
                            ListItem::new(content).style(Style::default().fg(Color::Green))
                        })
                        .collect();

                    let list = List::new(items)
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .border_style(Style::default().fg(if is_list_active { DEBIAN_RED } else { Color::DarkGray }))
                                .title(Line::from(" Installed List ").style(Style::default().fg(if is_list_active { Color::White } else { Color::DarkGray })))
                        )
                        .highlight_style(
                            Style::default()
                                .bg(DEBIAN_RED)
                                .fg(Color::White)
                                .add_modifier(Modifier::BOLD),
                        )
                        .highlight_symbol("> ");

                    f.render_stateful_widget(list, main_chunks[0], &mut app.installed_list_state);

                    let selected_desc = match app.installed_list_state.selected() {
                        Some(i) if !app.filtered_installed_packages.is_empty() => {
                            let pkg = &app.filtered_installed_packages[i];
                            format!(
                                "Name: {}\nSystem ID: {}\nSource: {:?}\nStatus: Installed\n\nDescription:\n{}",
                                pkg.display_name, pkg.name, pkg.source, pkg.description
                            )
                        }
                        _ => "No installed package selected.".to_string(),
                    };

                    let detail_box = Paragraph::new(selected_desc)
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .border_style(Style::default().fg(DEBIAN_RED))
                                .title(Line::from(" Details ").style(Style::default().fg(Color::White)))
                        );
                    f.render_widget(detail_box, main_chunks[1]);

                    let footer_text = match app.active_panel {
                        ActivePanel::InstalledSearchInput => " [↓] Focus List | [F3] Toggle Source | [Tab] Switch Panel | [Esc] Main Menu ",
                        ActivePanel::InstalledList => " [↑/↓] Navigate | [r] Remove Package | [s/F3] Toggle Source | [Tab] Switch Panel | [Esc] Main Menu ",
                        _ => " [Esc] Main Menu ",
                    };
                    let footer = Paragraph::new("")
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .border_style(Style::default().fg(DEBIAN_RED))
                                .title(Line::from(footer_text).style(Style::default().fg(Color::White)))
                        );
                    f.render_widget(footer, chunks[2]);
                }
            }

            // Render confirmation dialog pop-up if requested
            if let Some(ref pkg) = app.pending_remove {
                let area = centered_rect(50, 30, f.size());
                f.render_widget(Clear, area);

                let text = format!(
                    "\n  Are you sure you want to remove this package?\n\n  Name: {}\n  Source: {:?}\n\n  [Y / Enter] Confirm    [N / Esc] Cancel",
                    pkg.display_name, pkg.source
                );

                let popup = Paragraph::new(text)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(DEBIAN_RED))
                            .title(Line::from(" Confirm Package Removal ").style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)))
                    )
                    .style(Style::default().fg(Color::White));

                f.render_widget(popup, area);
            }
        })?;

        // Event Handling (Keyboard & Mouse)
        if event::poll(Duration::from_millis(30))? {
            match event::read()? {
                Event::Key(key) => {
                    if key.kind == KeyEventKind::Press {
                        // Priority handling for confirmation dialog
                        if let Some(pkg) = app.pending_remove.clone() {
                            match key.code {
                                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                                    app.pending_remove = None;
                                    match app.current_screen {
                                        AppScreen::SearchInstall => {
                                            if remove_package(&pkg, &mut terminal)? {
                                                if let Some(p) = app.search_packages.iter_mut().find(|p| p.name == pkg.name) {
                                                    p.installed = false;
                                                }
                                            }
                                        }
                                        AppScreen::InstalledPackages => {
                                            if remove_package(&pkg, &mut terminal)? {
                                                app.installed_packages.retain(|p| p.name != pkg.name);
                                                app.filter_installed();
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                                    app.pending_remove = None;
                                }
                                _ => {}
                            }
                            continue;
                        }

                        match app.current_screen {
                            AppScreen::MainMenu => match key.code {
                                KeyCode::Esc | KeyCode::Char('q') => break,
                                KeyCode::Down | KeyCode::Char('j') => app.next_menu_item(),
                                KeyCode::Up | KeyCode::Char('k') => app.prev_menu_item(),
                                KeyCode::Enter => {
                                    match app.menu_state.selected() {
                                        Some(0) => {
                                            app.current_screen = AppScreen::SearchInstall;
                                        }
                                        Some(1) => {
                                            app.installed_packages = load_installed_packages();
                                            app.installed_query.clear();
                                            app.active_panel = ActivePanel::InstalledSearchInput;
                                            app.current_screen = AppScreen::InstalledPackages;
                                            app.filter_installed();
                                        }
                                        Some(2) => {
                                            run_system_upgrade(&mut terminal)?;
                                        }
                                        Some(3) => break,
                                        _ => {}
                                    }
                                }
                                _ => {}
                            },

                            AppScreen::SearchInstall => {
                                // Dynamic source filter trigger via F3
                                if key.code == KeyCode::F(3) {
                                    app.search_source_filter = app.search_source_filter.next();
                                    app.last_keystroke = Some(Instant::now());
                                    app.is_searching = true;
                                    continue;
                                }

                                match key.code {
                                    KeyCode::Esc => {
                                        app.current_screen = AppScreen::MainMenu;
                                    }
                                    KeyCode::Tab => {
                                        app.active_panel = match app.active_panel {
                                            ActivePanel::SearchInput => ActivePanel::PackageList,
                                            _ => ActivePanel::SearchInput,
                                        };
                                    }
                                    _ => match app.active_panel {
                                        ActivePanel::SearchInput => match key.code {
                                            KeyCode::Down => {
                                                app.active_panel = ActivePanel::PackageList;
                                                if app.search_list_state.selected().is_none() && !app.search_packages.is_empty() {
                                                    app.search_list_state.select(Some(0));
                                                }
                                            }
                                            KeyCode::Char(c) => {
                                                app.search_query.push(c);
                                                app.last_keystroke = Some(Instant::now());
                                                app.is_searching = true;
                                            }
                                            KeyCode::Backspace => {
                                                app.search_query.pop();
                                                app.last_keystroke = Some(Instant::now());
                                                app.is_searching = true;
                                            }
                                            _ => {}
                                        },
                                        ActivePanel::PackageList => match key.code {
                                            KeyCode::Char('s') | KeyCode::Char('S') => {
                                                app.search_source_filter = app.search_source_filter.next();
                                                app.last_keystroke = Some(Instant::now());
                                                app.is_searching = true;
                                            }
                                            KeyCode::Down | KeyCode::Char('j') => {
                                                if !app.search_packages.is_empty() {
                                                    let i = match app.search_list_state.selected() {
                                                        Some(i) => if i >= app.search_packages.len() - 1 { 0 } else { i + 1 },
                                                        None => 0,
                                                    };
                                                    app.search_list_state.select(Some(i));
                                                }
                                            }
                                            KeyCode::Up | KeyCode::Char('k') => {
                                                if !app.search_packages.is_empty() {
                                                    let i = match app.search_list_state.selected() {
                                                        Some(i) => if i == 0 { app.search_packages.len() - 1 } else { i - 1 },
                                                        None => 0,
                                                    };
                                                    app.search_list_state.select(Some(i));
                                                }
                                            }
                                            KeyCode::Enter => {
                                                if let Some(i) = app.search_list_state.selected() {
                                                    if !app.search_packages.is_empty() {
                                                        let pkg = app.search_packages[i].clone();
                                                        if install_package(&pkg, &mut terminal)? {
                                                            app.search_packages[i].installed = true;
                                                        }
                                                    }
                                                }
                                            }
                                            KeyCode::Char('r') => {
                                                if let Some(i) = app.search_list_state.selected() {
                                                    if !app.search_packages.is_empty() {
                                                        let pkg = app.search_packages[i].clone();
                                                        if pkg.installed {
                                                            app.pending_remove = Some(pkg);
                                                        }
                                                    }
                                                }
                                            }
                                            _ => {}
                                        },
                                        _ => {}
                                    },
                                }
                            },

                            AppScreen::InstalledPackages => {
                                // Dynamic source filter trigger via F3
                                if key.code == KeyCode::F(3) {
                                    app.installed_source_filter = app.installed_source_filter.next();
                                    app.filter_installed();
                                    continue;
                                }

                                match key.code {
                                    KeyCode::Esc => {
                                        app.current_screen = AppScreen::MainMenu;
                                    }
                                    KeyCode::Tab => {
                                        app.active_panel = match app.active_panel {
                                            ActivePanel::InstalledSearchInput => ActivePanel::InstalledList,
                                            _ => ActivePanel::InstalledSearchInput,
                                        };
                                    }
                                    _ => match app.active_panel {
                                        ActivePanel::InstalledSearchInput => match key.code {
                                            KeyCode::Down => {
                                                app.active_panel = ActivePanel::InstalledList;
                                                if app.installed_list_state.selected().is_none() && !app.filtered_installed_packages.is_empty() {
                                                    app.installed_list_state.select(Some(0));
                                                }
                                            }
                                            KeyCode::Char(c) => {
                                                app.installed_query.push(c);
                                                app.filter_installed();
                                            }
                                            KeyCode::Backspace => {
                                                app.installed_query.pop();
                                                app.filter_installed();
                                            }
                                            _ => {}
                                        },
                                        ActivePanel::InstalledList => match key.code {
                                            KeyCode::Char('s') | KeyCode::Char('S') => {
                                                app.installed_source_filter = app.installed_source_filter.next();
                                                app.filter_installed();
                                            }
                                            KeyCode::Down | KeyCode::Char('j') => {
                                                if !app.filtered_installed_packages.is_empty() {
                                                    let i = match app.installed_list_state.selected() {
                                                        Some(i) => if i >= app.filtered_installed_packages.len() - 1 { 0 } else { i + 1 },
                                                        None => 0,
                                                    };
                                                    app.installed_list_state.select(Some(i));
                                                }
                                            }
                                            KeyCode::Up | KeyCode::Char('k') => {
                                                if !app.filtered_installed_packages.is_empty() {
                                                    let i = match app.installed_list_state.selected() {
                                                        Some(i) => if i == 0 { app.filtered_installed_packages.len() - 1 } else { i - 1 },
                                                        None => 0,
                                                    };
                                                    app.installed_list_state.select(Some(i));
                                                }
                                            }
                                            KeyCode::Char('r') => {
                                                if let Some(i) = app.installed_list_state.selected() {
                                                    if !app.filtered_installed_packages.is_empty() {
                                                        let pkg = app.filtered_installed_packages[i].clone();
                                                        app.pending_remove = Some(pkg);
                                                    }
                                                }
                                            }
                                            _ => {}
                                        },
                                        _ => {}
                                    },
                                }
                            },
                        }
                    }
                }
                Event::Mouse(mouse) => {
                    if app.pending_remove.is_some() {
                        continue;
                    }

                    let col = mouse.column;
                    let row = mouse.row;
                    let size = terminal.size().unwrap_or_default();

                    let chunks = Layout::default()
                        .direction(Direction::Vertical)
                        .constraints([
                            Constraint::Length(3),
                            Constraint::Min(10),
                            Constraint::Length(3),
                        ])
                        .split(size);

                    match mouse.kind {
                        MouseEventKind::Down(MouseButton::Left) => {
                            match app.current_screen {
                                AppScreen::MainMenu => {
                                    let r = chunks[1];
                                    if col > r.x && col < r.x + r.width - 1 && row > r.y && row < r.y + r.height - 1 {
                                        let clicked_idx = (row - r.y - 1) as usize;
                                        if clicked_idx < 4 {
                                            app.menu_state.select(Some(clicked_idx));
                                        }
                                    }
                                }
                                AppScreen::SearchInstall => {
                                    if col >= chunks[0].x && col < chunks[0].x + chunks[0].width && row >= chunks[0].y && row < chunks[0].y + chunks[0].height {
                                        app.active_panel = ActivePanel::SearchInput;
                                    } else {
                                        let main_chunks = Layout::default()
                                            .direction(Direction::Horizontal)
                                            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                                            .split(chunks[1]);

                                        let r_list = main_chunks[0];
                                        if col > r_list.x && col < r_list.x + r_list.width - 1 && row > r_list.y && row < r_list.y + r_list.height - 1 {
                                            app.active_panel = ActivePanel::PackageList;
                                            let clicked_idx = (row - r_list.y - 1) as usize;
                                            if clicked_idx < app.search_packages.len() {
                                                app.search_list_state.select(Some(clicked_idx));
                                            }
                                        }
                                    }
                                }
                                AppScreen::InstalledPackages => {
                                    if col >= chunks[0].x && col < chunks[0].x + chunks[0].width && row >= chunks[0].y && row < chunks[0].y + chunks[0].height {
                                        app.active_panel = ActivePanel::InstalledSearchInput;
                                    } else {
                                        let main_chunks = Layout::default()
                                            .direction(Direction::Horizontal)
                                            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                                            .split(chunks[1]);

                                        let r_list = main_chunks[0];
                                        if col > r_list.x && col < r_list.x + r_list.width - 1 && row > r_list.y && row < r_list.y + r_list.height - 1 {
                                            app.active_panel = ActivePanel::InstalledList;
                                            let clicked_idx = (row - r_list.y - 1) as usize;
                                            if clicked_idx < app.filtered_installed_packages.len() {
                                                app.installed_list_state.select(Some(clicked_idx));
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        MouseEventKind::ScrollUp => {
                            match app.current_screen {
                                AppScreen::MainMenu => {
                                    app.prev_menu_item();
                                }
                                AppScreen::SearchInstall => {
                                    if !app.search_packages.is_empty() {
                                        let i = match app.search_list_state.selected() {
                                            Some(i) => if i == 0 { app.search_packages.len() - 1 } else { i - 1 },
                                            None => 0,
                                        };
                                        app.search_list_state.select(Some(i));
                                    }
                                }
                                AppScreen::InstalledPackages => {
                                    if !app.filtered_installed_packages.is_empty() {
                                        let i = match app.installed_list_state.selected() {
                                            Some(i) => if i == 0 { app.filtered_installed_packages.len() - 1 } else { i - 1 },
                                            None => 0,
                                        };
                                        app.installed_list_state.select(Some(i));
                                    }
                                }
                            }
                        }
                        MouseEventKind::ScrollDown => {
                            match app.current_screen {
                                AppScreen::MainMenu => {
                                    app.next_menu_item();
                                }
                                AppScreen::SearchInstall => {
                                    if !app.search_packages.is_empty() {
                                        let i = match app.search_list_state.selected() {
                                            Some(i) => if i >= app.search_packages.len() - 1 { 0 } else { i + 1 },
                                            None => 0,
                                        };
                                        app.search_list_state.select(Some(i));
                                    }
                                }
                                AppScreen::InstalledPackages => {
                                    if !app.filtered_installed_packages.is_empty() {
                                        let i = match app.installed_list_state.selected() {
                                            Some(i) => if i >= app.filtered_installed_packages.len() - 1 { 0 } else { i + 1 },
                                            None => 0,
                                        };
                                        app.installed_list_state.select(Some(i));
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    Ok(())
}
