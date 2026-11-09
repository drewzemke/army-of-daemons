mod dashboard;
mod flash;
mod detail;
mod report_view;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Padding, Paragraph, Wrap};
use std::path::Path;

use crate::config::{self, DaemonInstance};
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
    All,
}

impl TimeRange {
    fn secs(self) -> i64 {
        match self {
            Self::OneHour => 3600,
            Self::SixHours => 6 * 3600,
            Self::TwentyFourHours => 24 * 3600,
            Self::OneWeek => 7 * 86400,
            Self::All => i64::MAX,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::OneHour => "1h",
            Self::SixHours => "6h",
            Self::TwentyFourHours => "24h",
            Self::OneWeek => "1w",
            Self::All => "all",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::OneHour => Self::SixHours,
            Self::SixHours => Self::TwentyFourHours,
            Self::TwentyFourHours => Self::OneWeek,
            Self::OneWeek => Self::All,
            Self::All => Self::OneHour,
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
    flash: Option<flash::Flash>,
    instances: std::collections::HashMap<String, DaemonInstance>,
    /// set while waiting for an action key after `a`
    action_mode: Option<Vec<crate::actions::Runnable>>,
    cmd_tx: tokio::sync::mpsc::UnboundedSender<CmdResult>,
    cmd_rx: tokio::sync::mpsc::UnboundedReceiver<CmdResult>,
}

/// outcome of a background action or on_read hook
struct CmdResult {
    label: String,
    /// report to mark unread again if the command failed (on_read)
    rollback: Option<i64>,
    result: Result<()>,
}

impl App {
    fn new(db: Database, config_dir: std::path::PathBuf) -> Self {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
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
            flash: None,
            instances: Default::default(),
            action_mode: None,
            cmd_tx,
            cmd_rx,
        }
    }

    fn load_instances(&mut self) {
        let daemons = config::load_daemons(&self.config_dir).unwrap_or_default();
        self.instances = daemon::runner::expand_all(&daemons)
            .into_iter()
            .map(|i| (i.instance_id.clone(), i))
            .collect();
    }

    fn current_report_id(&self) -> Option<i64> {
        match self.view {
            View::Detail { .. } => self.detail.selected_report_id(),
            View::Report { report_id } => Some(report_id),
            _ => None,
        }
    }

    /// mark reports read, firing on_read in the background for ones that were unread
    fn mark_read(&mut self, ids: &[i64]) {
        for &id in ids {
            let Ok(Some(report)) = self.db.get_report(id) else { continue };
            if report.is_read {
                continue;
            }
            let _ = self.db.mark_read(id);
            let Some(instance) = self.instances.get(&report.instance_id) else { continue };
            let Some(ref on_read) = instance.config.on_read else { continue };
            let ctx = config::template::build_report_context(instance, &report);
            self.spawn_cmd("on_read".into(), on_read.clone(), ctx, report.working_dir.clone(), Some(id));
        }
    }

    fn spawn_cmd(
        &self,
        label: String,
        command: String,
        ctx: minijinja::Value,
        working_dir: Option<String>,
        rollback: Option<i64>,
    ) {
        let tx = self.cmd_tx.clone();
        tokio::spawn(async move {
            let result = crate::actions::run_command(&command, &ctx, working_dir.as_deref()).await;
            let _ = tx.send(CmdResult { label, rollback, result });
        });
    }

    fn enter_action_mode(&mut self) {
        let Some(id) = self.current_report_id() else { return };
        let Ok(Some(report)) = self.db.get_report(id) else { return };
        let offers: Vec<crate::actions::Offer> = report.offered_actions.as_deref()
            .and_then(|j| serde_json::from_str(j).ok())
            .unwrap_or_default();
        let runnables = self.instances.get(&report.instance_id).map(|instance| {
            let ctx = config::template::build_report_context(instance, &report);
            crate::actions::runnables(&instance.config.actions, &offers, &ctx)
        }).unwrap_or_default();
        if runnables.is_empty() {
            self.flash = Some(flash::Flash::info("no actions for this report"));
        } else {
            self.flash = None;
            self.action_mode = Some(runnables);
        }
    }

    fn drain_cmd_results(&mut self) {
        while let Ok(res) = self.cmd_rx.try_recv() {
            self.flash = Some(match res.result {
                Ok(()) if res.rollback.is_some() => continue,
                Ok(()) => flash::Flash::info(format!("{} \u{2713}", res.label)),
                Err(e) => {
                    if let Some(id) = res.rollback {
                        let _ = self.db.mark_unread(id);
                        self.refresh();
                    }
                    flash::Flash::error(format!("{} failed: {e}", res.label))
                }
            });
        }
    }

    fn refresh(&mut self) {
        self.load_instances();
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

        app.drain_cmd_results();

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
    if app.flash.as_ref().is_some_and(|fl| fl.expired(std::time::Instant::now())) {
        app.flash = None;
    }
    let bar = if app.search_mode {
        Line::from(vec![
            Span::styled(" /", Style::default().fg(Color::Yellow)),
            Span::styled(&app.search_query, Style::default().fg(Color::White)),
            Span::styled("▌", Style::default().fg(Color::Yellow)),
        ])
    } else if let Some(ref fl) = app.flash {
        let color = match fl.level {
            flash::Level::Info => Color::DarkGray,
            flash::Level::Error => Color::Red,
        };
        Line::from(Span::styled(format!(" {}", fl.msg), Style::default().fg(color)))
    } else if let Some(ref runnables) = app.action_mode {
        let hints: Vec<String> = runnables.iter().map(|r| format!("{}:{}", r.key, r.label)).collect();
        Line::from(vec![
            Span::styled(" action ", Style::default().fg(Color::Black).bg(Color::Yellow)),
            Span::styled(format!("  {}  Esc:cancel", hints.join("  ")), Style::default().fg(Color::Yellow)),
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
            View::Dashboard => " j/k:navigate  Enter:drill in  r:read all  d:dry-run  t:time range  x:run now  /:search  ?:help  q:quit",
            View::Detail { .. } => " j/k:navigate  Enter:view  a:actions  D:delete  t:time range  r:read  u:unread  R:read all  /:search  y:copy  Esc:back",
            View::Report { .. } => " j/k:scroll  a:actions  D:delete  R:resume  s:source  y:copy  i:session id  r:read  u:unread  Esc:back",
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
        "r           mark read (all in dashboard, single in detail/report)",
        "u           mark report as unread",
        "R           mark all read (detail) / resume session (report)",
        "a           run an action on the report",
        "s           toggle source data",
        "y           copy report content",
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

    // errors stick until the next keypress
    if app.flash.as_ref().is_some_and(|fl| fl.level == flash::Level::Error) {
        app.flash = None;
    }

    if app.show_help {
        app.show_help = false;
        return;
    }

    if let Some(runnables) = app.action_mode.take() {
        if let KeyCode::Char(c) = key.code
            && let Some(r) = runnables.into_iter().find(|r| r.key == c)
        {
            let working_dir = app.current_report_id()
                .and_then(|id| app.db.get_report(id).ok().flatten())
                .and_then(|r| r.working_dir);
            app.spawn_cmd(r.name, r.command, r.ctx, working_dir, None);
        }
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
        KeyCode::Char('a') if matches!(app.view, View::Detail { .. } | View::Report { .. }) => {
            app.enter_action_mode();
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
        KeyCode::Char('r') => {
            if let Some(id) = app.dashboard.selected_instance_id() {
                let ids = app.db.unread_report_ids(&id).unwrap_or_default();
                app.mark_read(&ids);
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
            app.search_query.clear();
            app.view = View::Dashboard;
            app.refresh();
        }
        KeyCode::Char('j') | KeyCode::Down => app.detail.next(),
        KeyCode::Char('k') | KeyCode::Up => app.detail.prev(),
        KeyCode::Enter => {
            if let Some(id) = app.detail.selected_report_id() {
                app.mark_read(&[id]);
                app.search_query.clear();
                app.view = View::Report { report_id: id };
                app.refresh();
            }
        }
        KeyCode::Char('r') => {
            if let Some(id) = app.detail.selected_report_id() {
                app.mark_read(&[id]);
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
            let ids = app.db.unread_report_ids(instance_id).unwrap_or_default();
            app.mark_read(&ids);
            app.refresh();
        }
        KeyCode::Char('y') => {
            if let Some(id) = app.detail.selected_report_id()
                && let Ok(Some(report)) = app.db.get_report(id)
            {
                copy_to_clipboard(app, &report.format_text());
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
            app.mark_read(&[report_id]);
            app.refresh();
        }
        KeyCode::Char('u') => {
            let _ = app.db.mark_unread(report_id);
            app.refresh();
        }
        KeyCode::Char('y') => {
            if let Some(text) = app.report.report.as_ref().map(|r| r.format_text()) {
                copy_to_clipboard(app, &text);
            }
        }
        KeyCode::Char('i') => {
            if let Some(sid) = app.report.report.as_ref().and_then(|r| r.session_id.clone()) {
                copy_to_clipboard(app, &sid);
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
            if let Some(text) = app.dry_run.as_ref().map(|s| s.content.clone()) {
                copy_to_clipboard(app, &text);
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

fn copy_to_clipboard(app: &mut App, text: &str) {
    let result = arboard::Clipboard::new().and_then(|mut cb| cb.set_text(text));
    app.flash = Some(match result {
        Ok(()) => flash::Flash::info("copied"),
        Err(e) => {
            tracing::warn!(err=%e, "clipboard unavailable");
            flash::Flash::error(format!("copy failed: {e}"))
        }
    });
}
