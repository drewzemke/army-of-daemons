mod dashboard;
mod detail;
mod report_view;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Padding, Paragraph, Wrap};
use std::path::Path;

use crate::config;
use crate::daemon;
use crate::db::Database;

#[derive(Debug, Clone, PartialEq)]
enum View {
    Dashboard,
    Detail { instance_id: String },
    Report { report_id: i64 },
    DryRun,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum TimeRange {
    OneHour,
    SixHours,
    TwentyFourHours,
    OneWeek,
}

impl TimeRange {
    fn secs(self) -> i64 {
        match self {
            Self::OneHour => 3600,
            Self::SixHours => 6 * 3600,
            Self::TwentyFourHours => 24 * 3600,
            Self::OneWeek => 7 * 86400,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::OneHour => "1h",
            Self::SixHours => "6h",
            Self::TwentyFourHours => "24h",
            Self::OneWeek => "1w",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::OneHour => Self::SixHours,
            Self::SixHours => Self::TwentyFourHours,
            Self::TwentyFourHours => Self::OneWeek,
            Self::OneWeek => Self::OneHour,
        }
    }
}

struct DryRunState {
    instance_id: String,
    content: String,
    scroll: u16,
}

pub struct App {
    db: Database,
    config_dir: std::path::PathBuf,
    time_range: TimeRange,
    view: View,
    dashboard: dashboard::DashboardState,
    detail: detail::DetailState,
    report: report_view::ReportState,
    dry_run: Option<DryRunState>,
    dry_run_rx: Option<tokio::sync::oneshot::Receiver<Result<daemon::pipeline::DryRunOutput>>>,
    runner_healthy: bool,
    show_help: bool,
    should_quit: bool,
    search_mode: bool,
    search_query: String,
    pending_resume: Option<(String, Option<String>)>,
}

impl App {
    fn new(db: Database, config_dir: std::path::PathBuf) -> Self {
        Self {
            db,
            config_dir,
            time_range: TimeRange::OneHour,
            view: View::Dashboard,
            dashboard: dashboard::DashboardState::default(),
            detail: detail::DetailState::default(),
            report: report_view::ReportState::default(),
            dry_run: None,
            dry_run_rx: None,
            runner_healthy: true,
            show_help: false,
            should_quit: false,
            search_mode: false,
            search_query: String::new(),
            pending_resume: None,
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

        let cutoff = self.time_range.secs();
        let q = &self.search_query;
        match &self.view {
            View::Dashboard => self.dashboard.refresh(&self.db, cutoff, q),
            View::Detail { instance_id } => self.detail.refresh(&self.db, instance_id, cutoff, q),
            View::Report { report_id } => self.report.refresh(&self.db, *report_id),
            View::DryRun => {}
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

    let mut app = App::new(db, config_dir.to_path_buf());
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

        // check for dry-run completion
        if let Some(ref mut rx) = app.dry_run_rx
            && let Ok(result) = rx.try_recv()
        {
            app.dry_run_rx = None;
            match result {
                Ok(output) => {
                    let header = format!(
                        "{}: {} raw items, {} after filter\n\n",
                        output.instance_id, output.raw_count, output.filtered_items.len()
                    );
                    let json = serde_json::to_string_pretty(&output.filtered_items)
                        .unwrap_or_else(|_| "[]".into());
                    app.dry_run = Some(DryRunState {
                        instance_id: output.instance_id,
                        content: format!("{header}{json}"),
                        scroll: 0,
                    });
                    app.view = View::DryRun;
                }
                Err(e) => {
                    app.dry_run = Some(DryRunState {
                        instance_id: String::new(),
                        content: format!("dry-run failed: {e}"),
                        scroll: 0,
                    });
                    app.view = View::DryRun;
                }
            }
        }

        if let Some((session_id, working_dir)) = app.pending_resume.take() {
            // suspend TUI
            disable_raw_mode()?;
            crossterm::execute!(std::io::stdout(), LeaveAlternateScreen)?;

            let mut cmd = std::process::Command::new("claude");
            cmd.arg("--resume").arg(&session_id);
            if let Some(ref dir) = working_dir {
                cmd.current_dir(config::resolve_path(dir));
            }
            let _ = cmd.status();

            // restore TUI
            enable_raw_mode()?;
            crossterm::execute!(std::io::stdout(), EnterAlternateScreen)?;
            terminal.clear()?;
            app.refresh();
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
            View::Dashboard => dashboard::draw_with_range(f, main_area, &mut app.dashboard, app.time_range.label()),
            View::Detail { .. } => detail::draw(f, main_area, &mut app.detail),
            View::Report { .. } => report_view::draw(f, main_area, &app.report),
            View::DryRun => draw_dry_run(f, main_area, &app.dry_run),
        }
    }

    // status bar
    let status_bar = chunks[chunk_idx];
    let bar = if app.search_mode {
        Line::from(vec![
            Span::styled(" /", Style::default().fg(Color::Yellow)),
            Span::styled(&app.search_query, Style::default().fg(Color::White)),
            Span::styled("▌", Style::default().fg(Color::Yellow)),
        ])
    } else if !app.search_query.is_empty() {
        let hint = match &app.view {
            View::Dashboard => " j/k:navigate  Enter:drill in  /:search  Esc:clear",
            View::Detail { .. } => " j/k:navigate  Enter:view  /:search  Esc:clear+back",
            View::Report { .. } | View::DryRun => "",
        };
        Line::from(vec![
            Span::styled(format!(" filter: {} ", app.search_query), Style::default().fg(Color::Yellow)),
            Span::styled(hint, Style::default().fg(Color::DarkGray)),
        ])
    } else {
        let help_text = match &app.view {
            View::Dashboard => " j/k:navigate  Enter:drill in  d:dry-run  t:time range  x:run now  /:search  ?:help  q:quit",
            View::Detail { .. } => " j/k:navigate  Enter:view  D:delete  t:time range  r:read  u:unread  R:read all  /:search  y:copy  Esc:back",
            View::Report { .. } => " j/k:scroll  D:delete  R:resume  s:source  y:copy  i:session id  r:read  u:unread  Esc:back",
            View::DryRun => " j/k:scroll  y:copy  Esc:back",
        };
        Line::from(Span::styled(help_text, Style::default().fg(Color::DarkGray)))
    };
    f.render_widget(bar, status_bar);
}

fn draw_help(f: &mut Frame, area: Rect) {
    use ratatui::widgets::{Block, Borders, Paragraph};

    let help = vec![
        "j/k or ↑/↓  navigate",
        "Enter       drill in / open report",
        "Esc / q     back / quit",
        "d           dry-run daemon (dashboard)",
        "D           delete report",
        "r           mark report as read",
        "u           mark report as unread",
        "R           mark all read (this daemon)",
        "s           toggle source data",
        "y           copy report content",
        "R           resume claude session",
        "i           copy session id",
        "t           cycle time range (1h/6h/24h/1w)",
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
                app.refresh();
            }
            KeyCode::Enter => {
                app.search_mode = false;
            }
            KeyCode::Backspace => {
                app.search_query.pop();
                app.refresh();
            }
            KeyCode::Char(c) => {
                app.search_query.push(c);
                app.refresh();
            }
            _ => {}
        }
        return;
    }

    match key.code {
        KeyCode::Char('?') => app.show_help = true,
        KeyCode::Char('t') if !matches!(app.view, View::Report { .. }) => {
            app.time_range = app.time_range.next();
            app.refresh();
            return;
        }
        KeyCode::Char('/') if !matches!(app.view, View::Report { .. }) => {
            app.search_mode = true;
            return;
        }
        _ => {}
    }

    match &app.view.clone() {
        View::Dashboard => handle_dashboard_key(app, key),
        View::Detail { instance_id } => handle_detail_key(app, key, &instance_id.clone()),
        View::Report { report_id } => handle_report_key(app, key, *report_id),
        View::DryRun => handle_dry_run_key(app, key),
    }
}

fn handle_dashboard_key(app: &mut App, key: event::KeyEvent) {
    match key.code {
        KeyCode::Esc if !app.search_query.is_empty() => {
            app.search_query.clear();
            app.refresh();
        }
        KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
        KeyCode::Char('j') | KeyCode::Down => app.dashboard.next(),
        KeyCode::Char('k') | KeyCode::Up => app.dashboard.prev(),
        KeyCode::Enter => {
            if let Some(id) = app.dashboard.selected_instance_id() {
                app.search_query.clear();
                app.view = View::Detail { instance_id: id };
                app.refresh();
            }
        }
        KeyCode::Char('d') => {
            if app.dry_run_rx.is_some() {
                return;
            }
            if let Some(id) = app.dashboard.selected_instance_id() {
                let config_dir = app.config_dir.clone();
                let (tx, rx) = tokio::sync::oneshot::channel();
                app.dry_run_rx = Some(rx);
                tokio::spawn(async move {
                    let result = async {
                        let daemons = config::load_daemons(&config_dir)?;
                        let instances = daemon::runner::expand_all(&daemons);
                        let instance = instances.into_iter()
                            .find(|i| i.instance_id == id || i.daemon_name == id)
                            .ok_or_else(|| anyhow::anyhow!("instance '{id}' not found in configs"))?;
                        daemon::pipeline::dry_run_instance(&instance).await
                    }.await;
                    let _ = tx.send(result);
                });
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
            app.search_query.clear();
            app.view = View::Dashboard;
            app.refresh();
        }
        KeyCode::Char('j') | KeyCode::Down => app.detail.next(),
        KeyCode::Char('k') | KeyCode::Up => app.detail.prev(),
        KeyCode::Enter => {
            if let Some(id) = app.detail.selected_report_id() {
                let _ = app.db.mark_read(id);
                app.search_query.clear();
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
        KeyCode::Char('D') => {
            if let Some(id) = app.detail.selected_report_id() {
                let _ = app.db.delete_report(id);
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
        KeyCode::Char('i') => {
            if let Some(ref report) = app.report.report
                && let Some(ref sid) = report.session_id
            {
                copy_to_clipboard(sid);
            }
        }
        KeyCode::Char('D') => {
            if let Some(ref report) = app.report.report {
                let instance_id = report.instance_id.clone();
                let _ = app.db.delete_report(report_id);
                app.view = View::Detail { instance_id };
                app.refresh();
            }
        }
        KeyCode::Char('R') => {
            if let Some(ref report) = app.report.report
                && let Some(ref sid) = report.session_id
            {
                app.pending_resume = Some((sid.clone(), report.working_dir.clone()));
            }
        }
        KeyCode::Char('s') => app.report.toggle_source(),
        KeyCode::Char('j') | KeyCode::Down => app.report.scroll_down(),
        KeyCode::Char('k') | KeyCode::Up => app.report.scroll_up(),
        _ => {}
    }
}

fn handle_dry_run_key(app: &mut App, key: event::KeyEvent) {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.dry_run = None;
            app.view = View::Dashboard;
        }
        KeyCode::Char('y') => {
            if let Some(ref state) = app.dry_run {
                copy_to_clipboard(&state.content);
            }
        }
        KeyCode::Char('j') | KeyCode::Down => {
            if let Some(ref mut state) = app.dry_run {
                state.scroll = state.scroll.saturating_add(3);
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if let Some(ref mut state) = app.dry_run {
                state.scroll = state.scroll.saturating_sub(3);
            }
        }
        _ => {}
    }
}

fn draw_dry_run(f: &mut Frame, area: Rect, state: &Option<DryRunState>) {
    let Some(state) = state else {
        let p = Paragraph::new("running dry-run...")
            .block(Block::default().borders(Borders::ALL).title(" dry-run "));
        f.render_widget(p, area);
        return;
    };

    let title = if state.instance_id.is_empty() {
        " dry-run ".to_string()
    } else {
        format!(" dry-run: {} ", state.instance_id)
    };

    let lines: Vec<Line> = state.content.lines().map(|l| Line::from(l.to_string())).collect();
    let para = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(title).padding(Padding::horizontal(1)))
        .wrap(Wrap { trim: false })
        .scroll((state.scroll, 0));

    f.render_widget(para, area);
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
