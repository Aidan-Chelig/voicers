use ratatui::{
    prelude::*,
    widgets::{Block, BorderType, Borders},
};
use voicers_core::PeerTransportState;

pub fn transport_label(state: &PeerTransportState) -> &'static str {
    match state {
        PeerTransportState::Planned => "planned",
        PeerTransportState::Connecting => "connecting",
        PeerTransportState::Connected => "connected",
        PeerTransportState::Disconnected => "disconnected",
    }
}

pub fn panel_block<'a>(title: &'a str, accent: Color) -> Block<'a> {
    Block::default()
        .title(Line::from(vec![
            Span::styled(" ", Style::default().bg(accent)),
            Span::styled(
                format!(" {title} "),
                Style::default()
                    .fg(color_text())
                    .add_modifier(Modifier::BOLD),
            ),
        ]))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color_border()))
        .style(Style::default().bg(color_panel()).fg(color_text()))
}

pub fn badge(text: impl Into<String>, bg: Color, fg: Color) -> Span<'static> {
    Span::styled(
        format!(" {} ", text.into()),
        Style::default().bg(bg).fg(fg).add_modifier(Modifier::BOLD),
    )
}

pub fn label(text: &'static str) -> Span<'static> {
    Span::styled(
        text,
        Style::default()
            .fg(color_muted())
            .add_modifier(Modifier::BOLD),
    )
}

pub fn transport_color(state: &PeerTransportState) -> Color {
    match state {
        PeerTransportState::Planned => color_subtle(),
        PeerTransportState::Connecting => color_accent_soft(),
        PeerTransportState::Connected => color_good(),
        PeerTransportState::Disconnected => color_warn(),
    }
}

pub fn color_bg() -> Color {
    Color::Rgb(24, 26, 30)
}

pub fn color_panel() -> Color {
    Color::Rgb(32, 34, 40)
}

pub fn color_panel_alt() -> Color {
    Color::Rgb(43, 45, 49)
}

pub fn color_selected() -> Color {
    Color::Rgb(56, 60, 68)
}

pub fn color_border() -> Color {
    Color::Rgb(62, 65, 75)
}

pub fn color_text() -> Color {
    Color::Rgb(236, 238, 242)
}

pub fn color_muted() -> Color {
    Color::Rgb(153, 160, 174)
}

pub fn color_subtle() -> Color {
    Color::Rgb(125, 133, 150)
}

pub fn color_accent() -> Color {
    Color::Rgb(88, 101, 242)
}

pub fn color_accent_soft() -> Color {
    Color::Rgb(88, 166, 255)
}

pub fn color_good() -> Color {
    Color::Rgb(59, 165, 93)
}

pub fn color_warn() -> Color {
    Color::Rgb(237, 66, 69)
}
