mod dashboard;
mod detail;
mod report_view;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use ratatui::prelude::*;
use std::path::Path;

use crate::config;
use crate::db::Database;

#[derive(Debug, Clone, PartialEq)]
enum View {
    Dashboard,
    Detail { instance_id: String },
    Report { report_id: i64 },
}

pub struct App {
    db: Database,
    config_dir: std::path::PathBuf,
    display_after_days: u32,
    view: View,
    dashboard: dashboard::DashboardState,
    detail: detail::DetailState,
    report: report_view::ReportState,
    runner_healthy: bool,
    show_help: bool,
    should_quit: bool,
    search_mode: bool,
    search_query: String,
}

impl App {
    fn new(db: Database, config_dir: std::path::PathBuf, display_after_days: u32) -> Self {
        Self {
            db,
            config_dir,
            display_after_days,
            view: View::Dashboard,
            dashboard: dashboard::DashboardState::default(),
            detail: detail::DetailState::default(),
            report: report_view::ReportState::default(),
            runner_healthy: true,
            show_help: false,
            should_quit: false,
            search_mode: false,
            search_query: String::new(),
        }
    }

    fn refresh(&mut self) {
        // check runner heartbeat
        self.runner_healthy = match self.db.read_heartbeat() {
            Ok(Some(ts)) => {
                let age = chrono::Utc::now().timestamp() - ts;
                age < 90
            }
            _ => false,
        };

        match &self.view {
            View::Dashboard => self.dashboard.refresh(&self.db, self.display_after_days),
            View::Detail { instance_id } => self.detail.refresh(&self.db, instance_id, self.display_after_days),
            View::Report { report_id } => self.report.refresh(&self.db, *report_id),
        }
    }
}

pub async fn run(config_dir: &Path) -> Result<()> {
    let global = config::load_global(config_dir)?;
    let db = Database::open(&global.db_path)?;

    enable_raw_mode()?;
    crossterm::execute!(std::io::stdout(), EnterAlternateScreen)?;

    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(db, config_dir.to_path_buf(), global.display_after_days);
    app.refresh();

    let result = run_loop(&mut terminal, &mut app).await;

    disable_raw_mode()?;
    crossterm::execute!(std::io::stdout(), LeaveAlternateScreen)?;

    result
}

async fn run_loop(terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>, app: &mut App) -> Result<()> {
    let tick_rate = std::time::Duration::from_secs(2);
    let mut last_refresh = std::time::Instant::now();

    loop {
        terminal.draw(|f| draw(f, app))?;

        let timeout = tick_rate.saturating_sub(last_refresh.elapsed());
        if event::poll(timeout)?
            && let Event::Key(key) = event::read()?
        {
            handle_key(app, key);
        }

        if last_refresh.elapsed() >= tick_rate {
            app.refresh();
            last_refresh = std::time::Instant::now();
        }

        if app.should_quit {
            return Ok(());
        }
    }
}

fn draw(f: &mut Frame, app: &mut App) {
    let mut constraints = vec![];

    // runner health warning banner
    if !app.runner_healthy {
        constraints.push(Constraint::Length(1));
    }

    constraints.push(Constraint::Min(0));

    // help bar
    constraints.push(Constraint::Length(1));

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(f.area());

    let mut chunk_idx = 0;

    if !app.runner_healthy {
        let warning = Line::from(vec![
            Span::styled(" ⚠ runner not detected ", Style::default().fg(Color::Black).bg(Color::Yellow)),
            Span::styled(
                format!(" — is `{} run` running?", env!("CARGO_BIN_NAME")),
                Style::default().fg(Color::Yellow),
            ),
        ]);
        f.render_widget(warning, chunks[chunk_idx]);
        chunk_idx += 1;
    }

    let main_area = chunks[chunk_idx];
    chunk_idx += 1;

    // help overlay
    if app.show_help {
        draw_help(f, main_area);
    } else {
        match &app.view {
            View::Dashboard => dashboard::draw(f, main_area, &mut app.dashboard),
            View::Detail { .. } => detail::draw(f, main_area, &mut app.detail),
            View::Report { .. } => report_view::draw(f, main_area, &app.report),
        }
    }

    // status bar
    let status_bar = chunks[chunk_idx];
    let help_text = match &app.view {
        View::Dashboard => " j/k:navigate  Enter:drill in  x:run now  /:search  ?:help  q:quit",
        View::Detail { .. } => " j/k:navigate  Enter:view  r:read  u:unread  R:read all  y:copy  Esc:back",
        View::Report { .. } => " j/k:scroll  s:source  y:copy  r:read  u:unread  Esc:back",
    };
    let bar = Line::from(Span::styled(help_text, Style::default().fg(Color::DarkGray)));
    f.render_widget(bar, status_bar);
}

fn draw_help(f: &mut Frame, area: Rect) {
    use ratatui::widgets::{Block, Borders, Paragraph};

    let help = vec![
        "j/k or ↑/↓  navigate",
        "Enter       drill in / open report",
        "Esc / q     back / quit",
        "r           mark report as read",
        "u           mark report as unread",
        "R           mark all read (this daemon)",
        "s           toggle source data",
        "y           copy report content",
        "x           manually run daemon",
        "/           search / filter",
        "?           toggle this help",
    ];

    let text: Vec<Line> = help.iter().map(|l| Line::from(*l)).collect();
    let para = Paragraph::new(text)
        .block(Block::default().borders(Borders::ALL).title(" Help ").padding(ratatui::widgets::Padding::horizontal(1)));
    f.render_widget(para, area);
}

fn handle_key(app: &mut App, key: event::KeyEvent) {
    // ctrl-c always quits
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        app.should_quit = true;
        return;
    }

    if app.show_help {
        app.show_help = false;
        return;
    }

    if app.search_mode {
        match key.code {
            KeyCode::Esc => {
                app.search_mode = false;
                app.search_query.clear();
            }
            KeyCode::Enter => {
                app.search_mode = false;
                // apply filter (already stored in search_query)
            }
            KeyCode::Backspace => {
                app.search_query.pop();
            }
            KeyCode::Char(c) => {
                app.search_query.push(c);
            }
            _ => {}
        }
        return;
    }

    match key.code {
        KeyCode::Char('?') => app.show_help = true,
        KeyCode::Char('/') => app.search_mode = true,
        _ => {}
    }

    match &app.view.clone() {
        View::Dashboard => handle_dashboard_key(app, key),
        View::Detail { instance_id } => handle_detail_key(app, key, &instance_id.clone()),
        View::Report { report_id } => handle_report_key(app, key, *report_id),
    }
}

fn handle_dashboard_key(app: &mut App, key: event::KeyEvent) {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
        KeyCode::Char('j') | KeyCode::Down => app.dashboard.next(),
        KeyCode::Char('k') | KeyCode::Up => app.dashboard.prev(),
        KeyCode::Enter => {
            if let Some(id) = app.dashboard.selected_instance_id() {
                app.view = View::Detail { instance_id: id };
                app.refresh();
            }
        }
        KeyCode::Char('x') => {
            if let Some(id) = app.dashboard.selected_instance_id() {
                // spawn check in background
                let config_dir = app.config_dir.clone();
                tokio::spawn(async move {
                    let _ = tokio::process::Command::new(env!("CARGO_BIN_NAME"))
                        .arg("check")
                        .arg(&id)
                        .arg("--config-dir")
                        .arg(&config_dir)
                        .output()
                        .await;
                });
            }
        }
        _ => {}
    }
}

fn handle_detail_key(app: &mut App, key: event::KeyEvent, instance_id: &str) {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.view = View::Dashboard;
            app.refresh();
        }
        KeyCode::Char('j') | KeyCode::Down => app.detail.next(),
        KeyCode::Char('k') | KeyCode::Up => app.detail.prev(),
        KeyCode::Enter => {
            if let Some(id) = app.detail.selected_report_id() {
                let _ = app.db.mark_read(id);
                app.view = View::Report { report_id: id };
                app.refresh();
            }
        }
        KeyCode::Char('r') => {
            if let Some(id) = app.detail.selected_report_id() {
                let _ = app.db.mark_read(id);
                app.refresh();
            }
        }
        KeyCode::Char('u') => {
            if let Some(id) = app.detail.selected_report_id() {
                let _ = app.db.mark_unread(id);
                app.refresh();
            }
        }
        KeyCode::Char('R') => {
            let _ = app.db.mark_all_read(instance_id);
            app.refresh();
        }
        KeyCode::Char('y') => {
            if let Some(id) = app.detail.selected_report_id()
                && let Ok(Some(report)) = app.db.get_report(id)
            {
                copy_to_clipboard(&report.format_text());
            }
        }
        _ => {}
    }
}

fn handle_report_key(app: &mut App, key: event::KeyEvent, report_id: i64) {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            // go back to detail view
            if let Some(ref report) = app.report.report {
                let instance_id = report.instance_id.clone();
                app.view = View::Detail { instance_id };
            } else {
                app.view = View::Dashboard;
            }
            app.refresh();
        }
        KeyCode::Char('r') => {
            let _ = app.db.mark_read(report_id);
            app.refresh();
        }
        KeyCode::Char('u') => {
            let _ = app.db.mark_unread(report_id);
            app.refresh();
        }
        KeyCode::Char('y') => {
            if let Some(ref report) = app.report.report {
                copy_to_clipboard(&report.format_text());
            }
        }
        KeyCode::Char('s') => app.report.toggle_source(),
        KeyCode::Char('j') | KeyCode::Down => app.report.scroll_down(),
        KeyCode::Char('k') | KeyCode::Up => app.report.scroll_up(),
        _ => {}
    }
}

fn copy_to_clipboard(text: &str) {
    match arboard::Clipboard::new() {
        Ok(mut cb) => {
            let _ = cb.set_text(text);
        }
        Err(e) => {
            tracing::warn!(err=%e, "clipboard unavailable");
        }
    }
}
