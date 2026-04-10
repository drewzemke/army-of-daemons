use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Cell, Padding, Row, Table, TableState};

use crate::db::{self, DaemonSummary, Database};

#[derive(Default)]
pub struct DashboardState {
    pub summaries: Vec<DaemonSummary>,
    pub table_state: TableState,
}

impl DashboardState {
    pub fn refresh(&mut self, db: &Database) {
        self.summaries = db.daemon_summaries().unwrap_or_default();
        // keep selection in bounds
        if let Some(sel) = self.table_state.selected() {
            if sel >= self.summaries.len() && !self.summaries.is_empty() {
                self.table_state.select(Some(self.summaries.len() - 1));
            }
        } else if !self.summaries.is_empty() {
            self.table_state.select(Some(0));
        }
    }

    pub fn next(&mut self) {
        if self.summaries.is_empty() {
            return;
        }
        let i = self.table_state.selected().unwrap_or(0);
        let next = if i >= self.summaries.len() - 1 { 0 } else { i + 1 };
        self.table_state.select(Some(next));
    }

    pub fn prev(&mut self) {
        if self.summaries.is_empty() {
            return;
        }
        let i = self.table_state.selected().unwrap_or(0);
        let prev = if i == 0 { self.summaries.len() - 1 } else { i - 1 };
        self.table_state.select(Some(prev));
    }

    pub fn selected_instance_id(&self) -> Option<String> {
        self.table_state
            .selected()
            .and_then(|i| self.summaries.get(i))
            .map(|s| s.instance_id.clone())
    }
}

pub fn draw(f: &mut Frame, area: Rect, state: &mut DashboardState) {
    let header = Row::new(vec!["daemon", "status", "unread", "last check"])
        .style(Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD))
        .bottom_margin(1);

    let rows: Vec<Row> = state
        .summaries
        .iter()
        .map(|s| {
            let status = if s.unread_count == 0 {
                "\u{2705} ok".to_string()
            } else {
                let emoji = db::severity_emoji(s.max_severity);
                let label = db::severity_label(s.max_severity);
                format!("{emoji} {label}")
            };

            let age = s.last_checked.map(|ts| {
                let ago = chrono::Utc::now().timestamp() - ts;
                format_age(ago)
            }).unwrap_or_else(|| "never".into());

            Row::new(vec![
                Cell::from(truncate(&s.instance_id, 30)),
                Cell::from(status),
                Cell::from(s.unread_count.to_string()),
                Cell::from(age),
            ])
        })
        .collect();

    let widths = [
        Constraint::Min(30),
        Constraint::Length(14),
        Constraint::Length(8),
        Constraint::Length(12),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" army-of-daemons ")
                .padding(Padding::horizontal(1)),
        )
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    f.render_stateful_widget(table, area, &mut state.table_state);
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max - 1])
    }
}

fn format_age(secs: i64) -> String {
    if secs < 0 {
        return "just now".into();
    }
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}
