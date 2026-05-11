use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Padding, Paragraph, Wrap};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

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
    lines.extend(tui_markdown::from_str(&report.analysis).lines);

    // session footer
    if let Some(ref sid) = report.session_id {
        let dim = Style::default().fg(Color::DarkGray);
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("---", dim)));
        lines.push(Line::from(Span::styled(format!("session: {sid}"), dim)));
    }

    let block_title = format!(" #{} {} — {} ", report.id, report.instance_id, title_text);
    let para = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(block_title).padding(Padding::horizontal(1)))
        .wrap(Wrap { trim: false })
        .scroll((state.scroll, 0));

    f.render_widget(para, area);
}


fn highlight_json(json: &str) -> Text<'static> {
    let ps = SyntaxSet::load_defaults_newlines();
    let ts = ThemeSet::load_defaults();
    let Some(syntax) = ps.find_syntax_by_extension("json") else {
        return plain_text(json);
    };
    let Some(theme) = ts.themes.get("base16-ocean.dark") else {
        return plain_text(json);
    };
    let mut h = HighlightLines::new(syntax, theme);
    let mut lines = Vec::new();
    for line in LinesWithEndings::from(json) {
        let Ok(segments) = h.highlight_line(line, &ps) else {
            lines.push(Line::from(line.trim_end_matches('\n').to_string()));
            continue;
        };
        let spans: Vec<Span<'static>> = segments
            .into_iter()
            .map(|(style, text)| {
                let fg = Color::Rgb(style.foreground.r, style.foreground.g, style.foreground.b);
                let mut modifier = Modifier::empty();
                if style.font_style.contains(FontStyle::BOLD) {
                    modifier |= Modifier::BOLD;
                }
                if style.font_style.contains(FontStyle::ITALIC) {
                    modifier |= Modifier::ITALIC;
                }
                if style.font_style.contains(FontStyle::UNDERLINE) {
                    modifier |= Modifier::UNDERLINED;
                }
                Span::styled(
                    text.trim_end_matches('\n').to_string(),
                    Style::default().fg(fg).add_modifier(modifier),
                )
            })
            .collect();
        lines.push(Line::from(spans));
    }
    Text::from(lines)
}

fn plain_text(s: &str) -> Text<'static> {
    let dim = Style::default().fg(Color::DarkGray);
    Text::from(
        s.lines()
            .map(|l| Line::from(Span::styled(l.to_string(), dim)))
            .collect::<Vec<_>>(),
    )
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
        let pretty = serde_json::to_string_pretty(
            &serde_json::from_str::<serde_json::Value>(json).unwrap(),
        )
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
        let has_styled = text.lines.iter().any(|line| {
            line.spans
                .iter()
                .any(|s| s.style != Style::default())
        });
        assert!(has_styled, "markdown should produce styled spans");
    }
}

