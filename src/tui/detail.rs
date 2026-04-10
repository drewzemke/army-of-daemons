use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Cell, Padding, Row, Table, TableState};

use crate::db::{self, Database, Report};

#[derive(Default)]
pub struct DetailState {
    pub reports: Vec<Report>,
    pub instance_id: String,
    pub table_state: TableState,
}

impl DetailState {
    pub fn refresh(&mut self, db: &Database, instance_id: &str) {
        self.instance_id = instance_id.to_string();
        self.reports = db.list_reports(instance_id, 100).unwrap_or_default();
        if let Some(sel) = self.table_state.selected() {
            if sel >= self.reports.len() && !self.reports.is_empty() {
                self.table_state.select(Some(self.reports.len() - 1));
            }
        } else if !self.reports.is_empty() {
            self.table_state.select(Some(0));
        }
    }

    pub fn next(&mut self) {
        if self.reports.is_empty() {
            return;
        }
        let i = self.table_state.selected().unwrap_or(0);
        let next = if i >= self.reports.len() - 1 { 0 } else { i + 1 };
        self.table_state.select(Some(next));
    }

    pub fn prev(&mut self) {
        if self.reports.is_empty() {
            return;
        }
        let i = self.table_state.selected().unwrap_or(0);
        let prev = if i == 0 { self.reports.len() - 1 } else { i - 1 };
        self.table_state.select(Some(prev));
    }

    pub fn selected_report_id(&self) -> Option<i64> {
        self.table_state
            .selected()
            .and_then(|i| self.reports.get(i))
            .map(|r| r.id)
    }
}

pub fn draw(f: &mut Frame, area: Rect, state: &mut DetailState) {
    let header = Row::new(vec!["sev", "time", "title", "read"])
        .style(Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD))
        .bottom_margin(1);

    let rows: Vec<Row> = state
        .reports
        .iter()
        .map(|r| {
            let sev_str = format!(
                "{} {}",
                db::severity_emoji(r.severity),
                r.severity
            );

            let time = chrono::DateTime::from_timestamp(r.created_at, 0)
                .map(|dt| dt.format("%H:%M %b %d").to_string())
                .unwrap_or_else(|| "?".into());

            let title = r.title.as_deref().unwrap_or(
                r.dedup_key.as_deref().unwrap_or("(no title)")
            );

            let read_marker = if r.is_read { "●" } else { "○" };

            Row::new(vec![
                Cell::from(sev_str),
                Cell::from(time),
                Cell::from(title.to_string()),
                Cell::from(read_marker),
            ])
        })
        .collect();

    let widths = [
        Constraint::Length(6),
        Constraint::Length(14),
        Constraint::Min(20),
        Constraint::Length(4),
    ];

    let title = format!(" {} ", state.instance_id);
    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(title).padding(Padding::horizontal(1)))
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    f.render_stateful_widget(table, area, &mut state.table_state);
}
