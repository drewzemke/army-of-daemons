use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Padding, Paragraph, Wrap};

use crate::db::{self, Database, Report};

#[derive(Default)]
pub struct ReportState {
    pub report: Option<Report>,
    pub scroll: u16,
    pub show_source: bool,
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

    pub fn toggle_source(&mut self) {
        self.show_source = !self.show_source;
    }
}

pub fn draw(f: &mut Frame, area: Rect, state: &ReportState) {
    let Some(ref report) = state.report else {
        let p = Paragraph::new("report not found").block(Block::default().borders(Borders::ALL));
        f.render_widget(p, area);
        return;
    };

    // build header + body
    let sev_label = db::severity_label(report.severity);
    let sev_emoji = db::severity_emoji(report.severity);
    let time = chrono::DateTime::from_timestamp(report.created_at, 0)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default();

    let title_text = report.title.as_deref().unwrap_or("report");

    let mut lines: Vec<Line> = Vec::new();

    // severity + time header
    if report.is_error {
        lines.push(Line::from(vec![
            Span::styled(
                "\u{274c} daemon error ",
                Style::default()
                    .fg(Color::Red)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(time, Style::default().fg(Color::DarkGray)),
        ]));
    } else {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{sev_emoji} {sev_label} ({}) ", report.severity),
                Style::default()
                    .fg(severity_color(report.severity))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(time, Style::default().fg(Color::DarkGray)),
        ]));
    }
    lines.push(Line::from(""));

    // source data section
    if report.source_items_json.is_some() {
        let dim = Style::default().fg(Color::DarkGray);
        if state.show_source {
            lines.push(Line::from(Span::styled(
                "── Source Data (s to collapse) ──",
                dim.add_modifier(Modifier::BOLD),
            )));
            if let Some(ref raw) = report.source_items_json {
                let pretty = serde_json::from_str::<serde_json::Value>(raw)
                    .and_then(|v| serde_json::to_string_pretty(&v))
                    .unwrap_or_else(|_| raw.clone());
                lines.extend(highlight_json(&pretty).lines);
            }
        } else {
            lines.push(Line::from(Span::styled(
                "── Source Data (s to expand) ──",
                dim.add_modifier(Modifier::BOLD),
            )));
        }
        lines.push(Line::from(Span::styled("---", dim)));
        lines.push(Line::from(""));
    }

    // analysis body
    let md_opts = tui_markdown::Options::new(TermStyleSheet);
    let mut md_text = tui_markdown::from_str_with_options(&report.analysis, &md_opts);
    color_inline_styles(&mut md_text);
    lines.extend(md_text.lines);

    // session footer
    if let Some(ref sid) = report.session_id {
        let dim = Style::default().fg(Color::DarkGray);
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("---", dim)));
        lines.push(Line::from(Span::styled(format!("session: {sid}"), dim)));
    }

    let block_title = format!(" #{} {} — {} ", report.id, report.instance_id, title_text);
    let para = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(block_title)
                .padding(Padding::horizontal(1)),
        )
        .wrap(Wrap { trim: false })
        .scroll((state.scroll, 0));

    f.render_widget(para, area);
}

#[derive(Clone)]
struct TermStyleSheet;

impl tui_markdown::StyleSheet for TermStyleSheet {
    fn heading(&self, level: u8) -> Style {
        match level {
            1 => Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::BOLD),
            2 => Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
            3 => Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
            _ => Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        }
    }
    fn code(&self) -> Style {
        Style::default().fg(Color::Green)
    }
    fn link(&self) -> Style {
        Style::default()
            .fg(Color::Blue)
            .add_modifier(Modifier::UNDERLINED)
    }
    fn blockquote(&self) -> Style {
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC)
    }
    fn heading_meta(&self) -> Style {
        Style::default().fg(Color::DarkGray)
    }
    fn metadata_block(&self) -> Style {
        Style::default().fg(Color::DarkGray)
    }
}

fn color_inline_styles(text: &mut Text<'_>) {
    for line in &mut text.lines {
        for span in &mut line.spans {
            if span.style.fg.is_some() {
                continue;
            }
            if span.style.add_modifier.contains(Modifier::BOLD) {
                span.style.fg = Some(Color::Cyan);
            } else if span.style.add_modifier.contains(Modifier::ITALIC) {
                span.style.fg = Some(Color::Yellow);
            }
        }
    }
}

fn highlight_json(json: &str) -> Text<'static> {
    let mut lines = Vec::new();
    let mut in_string = false;
    for raw_line in json.lines() {
        let mut spans = Vec::new();
        let mut chars = raw_line.chars().peekable();
        let mut buf = String::new();

        while let Some(&ch) = chars.peek() {
            if in_string {
                buf.push(chars.next().unwrap());
                if ch == '\\' {
                    if let Some(&esc) = chars.peek() {
                        buf.push(chars.next().unwrap());
                        let _ = esc;
                    }
                } else if ch == '"' {
                    // check if this string was a key (next non-ws char is ':')
                    let rest: String = chars.clone().collect();
                    let is_key = rest.trim_start().starts_with(':');
                    let color = if is_key { Color::Blue } else { Color::Green };
                    spans.push(Span::styled(buf.clone(), Style::default().fg(color)));
                    buf.clear();
                    in_string = false;
                }
            } else {
                match ch {
                    '"' => {
                        if !buf.is_empty() {
                            spans.push(classify_token(&buf));
                            buf.clear();
                        }
                        buf.push(chars.next().unwrap());
                        in_string = true;
                    }
                    '{' | '}' | '[' | ']' | ':' | ',' => {
                        if !buf.is_empty() {
                            spans.push(classify_token(&buf));
                            buf.clear();
                        }
                        spans.push(Span::styled(
                            ch.to_string(),
                            Style::default().fg(Color::DarkGray),
                        ));
                        chars.next();
                    }
                    _ => {
                        buf.push(chars.next().unwrap());
                    }
                }
            }
        }
        if !buf.is_empty() {
            spans.push(classify_token(&buf));
        }
        lines.push(Line::from(spans));
    }
    Text::from(lines)
}

fn classify_token(token: &str) -> Span<'static> {
    let trimmed = token.trim();
    let style = if trimmed == "true" || trimmed == "false" || trimmed == "null" {
        Style::default().fg(Color::Cyan)
    } else if trimmed.parse::<f64>().is_ok() {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    };
    Span::styled(token.to_string(), style)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlight_json_produces_colored_spans() {
        let json = r#"{"name": "test", "count": 42, "active": true}"#;
        let pretty =
            serde_json::to_string_pretty(&serde_json::from_str::<serde_json::Value>(json).unwrap())
                .unwrap();

        let text = highlight_json(&pretty);

        assert!(!text.lines.is_empty(), "should produce lines");

        // at least one line should have multiple spans (proving tokenization,
        // not just one-span-per-line like the old DarkGray fallback)
        let multi_span = text.lines.iter().any(|line| line.spans.len() > 1);
        assert!(multi_span, "highlighted JSON should have multi-span lines");
    }

    #[test]
    fn highlight_json_handles_invalid_input() {
        let bad = "this is { not valid json @@";
        let text = highlight_json(bad);
        // should not panic, should return some text
        assert!(!text.lines.is_empty());
    }

    #[test]
    fn markdown_rendering_produces_styled_output() {
        let md = "# Title\n\nSome text with **bold**.\n\n## Subtitle\n\n- item 1\n- item 2\n\n```json\n{\"a\": 1}\n```\n";
        let text = tui_markdown::from_str(md);

        assert!(!text.lines.is_empty(), "should produce lines from markdown");

        // the title line should have some styling (bold, color, etc.)
        let has_styled = text
            .lines
            .iter()
            .any(|line| line.spans.iter().any(|s| s.style != Style::default()));
        assert!(has_styled, "markdown should produce styled spans");
    }
}
