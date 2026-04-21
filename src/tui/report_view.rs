use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Padding, Paragraph, Wrap};

use crate::db::{self, Database, Report};

#[derive(Default)]
pub struct ReportState {
    pub report: Option<Report>,
    pub scroll: u16,
}

impl ReportState {
    pub fn refresh(&mut self, db: &Database, report_id: i64) {
        let is_same = self.report.as_ref().is_some_and(|r| r.id == report_id);
        self.report = db.get_report(report_id).unwrap_or(None);
        if !is_same {
            self.scroll = 0;
        }
    }

    pub fn scroll_down(&mut self) {
        self.scroll = self.scroll.saturating_add(3);
    }

    pub fn scroll_up(&mut self) {
        self.scroll = self.scroll.saturating_sub(3);
    }
}

pub fn draw(f: &mut Frame, area: Rect, state: &ReportState) {
    let Some(ref report) = state.report else {
        let p = Paragraph::new("report not found")
            .block(Block::default().borders(Borders::ALL));
        f.render_widget(p, area);
        return;
    };

    // build header + body
    let sev_label = db::severity_label(report.severity);
    let sev_emoji = db::severity_emoji(report.severity);
    let time = chrono::DateTime::from_timestamp(report.created_at, 0)
        .map(|dt| dt.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default();

    let title_text = report.title.as_deref().unwrap_or("report");

    let mut lines: Vec<Line> = Vec::new();

    // severity + time header
    lines.push(Line::from(vec![
        Span::styled(
            format!("{sev_emoji} {sev_label} ({}) ", report.severity),
            Style::default().fg(severity_color(report.severity)).add_modifier(Modifier::BOLD),
        ),
        Span::styled(time, Style::default().fg(Color::DarkGray)),
    ]));
    lines.push(Line::from(""));

    if report.is_error {
        lines.push(Line::from(Span::styled(
            "⚠ daemon error",
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));
    }

    // analysis body
    for line in report.analysis.lines() {
        let styled = style_markdown_line(line);
        lines.push(styled);
    }

    let block_title = format!(" {} — {} ", report.instance_id, title_text);
    let para = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(block_title).padding(Padding::horizontal(1)))
        .wrap(Wrap { trim: false })
        .scroll((state.scroll, 0));

    f.render_widget(para, area);
}

/// minimal markdown styling — headers get bold, code blocks get dim
fn style_markdown_line(line: &str) -> Line<'static> {
    if line.starts_with("## ") || line.starts_with("### ") {
        Line::from(Span::styled(
            line.to_string(),
            Style::default().add_modifier(Modifier::BOLD).fg(Color::Cyan),
        ))
    } else if line.starts_with("# ") {
        Line::from(Span::styled(
            line.to_string(),
            Style::default().add_modifier(Modifier::BOLD).fg(Color::White),
        ))
    } else if line.starts_with("```") {
        Line::from(Span::styled(
            line.to_string(),
            Style::default().fg(Color::DarkGray),
        ))
    } else if line.starts_with("- ") || line.starts_with("* ") {
        Line::from(Span::styled(
            line.to_string(),
            Style::default().fg(Color::White),
        ))
    } else {
        Line::from(line.to_string())
    }
}

fn severity_color(sev: i32) -> Color {
    match sev {
        5 => Color::Red,
        4 => Color::Rgb(255, 165, 0),
        3 => Color::Yellow,
        2 => Color::Blue,
        _ => Color::DarkGray,
    }
}
