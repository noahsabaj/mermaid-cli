use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Gauge, Paragraph},
    Frame,
};

use super::types::HardwareStats;

/// Render the diagnostics panel
pub fn render_diagnostics_panel(frame: &mut Frame, area: Rect, stats: &HardwareStats) {
    // Create centered panel
    let panel_width = 50.min(area.width);
    let panel_height = 20.min(area.height);

    let x = (area.width.saturating_sub(panel_width)) / 2;
    let y = (area.height.saturating_sub(panel_height)) / 2;

    let panel_area = Rect::new(
        area.x + x,
        area.y + y,
        panel_width,
        panel_height,
    );

    // Create layout for panel contents
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(5),  // GPU section
            Constraint::Length(3),  // Model section
            Constraint::Length(3),  // Performance section
            Constraint::Length(3),  // System section
            Constraint::Min(1),     // Help text
        ])
        .split(panel_area);

    // Panel border
    let panel_block = Block::default()
        .title(" Hardware Diagnostics ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    frame.render_widget(panel_block, panel_area);

    // GPU Section
    if let Some(gpu) = &stats.gpu {
        render_gpu_section(frame, chunks[0], gpu);
    } else {
        let no_gpu = Paragraph::new("No GPU detected")
            .style(Style::default().fg(Color::DarkGray));
        frame.render_widget(no_gpu, chunks[0]);
    }

    // Model Section
    render_model_section(frame, chunks[1], stats);

    // Performance Section
    render_performance_section(frame, chunks[2], stats);

    // System Section
    render_system_section(frame, chunks[3], stats);

    // Help text
    let help = Paragraph::new(Line::from(vec![
        Span::raw("Press "),
        Span::styled("F2", Style::default().fg(Color::Yellow)),
        Span::raw(" or "),
        Span::styled("Esc", Style::default().fg(Color::Yellow)),
        Span::raw(" to close"),
    ]))
    .alignment(Alignment::Center)
    .style(Style::default().fg(Color::DarkGray));

    frame.render_widget(help, chunks[4]);
}

/// Render GPU section
fn render_gpu_section(frame: &mut Frame, area: Rect, gpu: &super::types::GpuInfo) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),  // GPU name
            Constraint::Length(1),  // Usage gauge
            Constraint::Length(1),  // VRAM gauge
            Constraint::Length(1),  // Temperature
        ])
        .split(area);

    // GPU name and type
    let gpu_name = Paragraph::new(format!("GPU: {} [{}]", gpu.name, gpu.gpu_type.display_name()))
        .style(Style::default().fg(Color::Green).add_modifier(Modifier::BOLD));
    frame.render_widget(gpu_name, chunks[0]);

    // GPU usage gauge
    let usage_color = get_usage_color(gpu.usage_percent);
    let usage_gauge = Gauge::default()
        .block(Block::default().title(format!("Usage: {:.0}%", gpu.usage_percent)))
        .gauge_style(Style::default().fg(usage_color))
        .percent(gpu.usage_percent as u16)
        .label("");
    frame.render_widget(usage_gauge, chunks[1]);

    // VRAM gauge
    let vram_percent = (gpu.memory_used_gb / gpu.memory_total_gb * 100.0).min(100.0);
    let vram_color = get_usage_color(vram_percent);
    let vram_gauge = Gauge::default()
        .block(Block::default().title(format!(
            "VRAM: {:.1}GB / {:.1}GB",
            gpu.memory_used_gb, gpu.memory_total_gb
        )))
        .gauge_style(Style::default().fg(vram_color))
        .percent(vram_percent as u16)
        .label("");
    frame.render_widget(vram_gauge, chunks[2]);

    // Temperature if available
    if let Some(temp) = gpu.temperature_celsius {
        let temp_color = if temp > 85.0 { Color::Red }
                        else if temp > 75.0 { Color::Yellow }
                        else { Color::Green };

        let temp_text = Paragraph::new(format!("Temperature: {:.0}°C", temp))
            .style(Style::default().fg(temp_color));
        frame.render_widget(temp_text, chunks[3]);
    }
}

/// Render model section
fn render_model_section(frame: &mut Frame, area: Rect, stats: &HardwareStats) {
    if let Some(model) = &stats.model_info {
        let lines = vec![
            Line::from(vec![
                Span::raw("Model: "),
                Span::styled(&model.name, Style::default().fg(Color::Cyan)),
            ]),
            Line::from(format!(
                "Context: {} / {} tokens",
                model.context_used, model.context_length
            )),
        ];

        let model_info = Paragraph::new(lines);
        frame.render_widget(model_info, area);
    } else {
        let no_model = Paragraph::new("No model loaded")
            .style(Style::default().fg(Color::DarkGray));
        frame.render_widget(no_model, area);
    }
}

/// Render performance section
fn render_performance_section(frame: &mut Frame, area: Rect, stats: &HardwareStats) {
    let mut lines = vec![];

    if let Some(speed) = stats.inference_speed {
        let speed_color = if speed > 50.0 { Color::Green }
                         else if speed > 20.0 { Color::Yellow }
                         else { Color::Red };

        lines.push(Line::from(vec![
            Span::raw("Inference: "),
            Span::styled(
                format!("{:.1} tokens/sec", speed),
                Style::default().fg(speed_color),
            ),
        ]));
    } else {
        lines.push(Line::from(
            Span::styled("No active inference", Style::default().fg(Color::DarkGray))
        ));
    }

    let performance = Paragraph::new(lines);
    frame.render_widget(performance, area);
}

/// Render system section
fn render_system_section(frame: &mut Frame, area: Rect, stats: &HardwareStats) {
    let cpu_color = get_usage_color(stats.cpu_usage_percent);
    let ram_percent = (stats.ram_used_gb / stats.ram_total_gb * 100.0).min(100.0);
    let ram_color = get_usage_color(ram_percent);

    let lines = vec![
        Line::from(vec![
            Span::raw("CPU: "),
            Span::styled(
                format!("{:.0}%", stats.cpu_usage_percent),
                Style::default().fg(cpu_color),
            ),
        ]),
        Line::from(vec![
            Span::raw("RAM: "),
            Span::styled(
                format!("{:.1}GB / {:.1}GB", stats.ram_used_gb, stats.ram_total_gb),
                Style::default().fg(ram_color),
            ),
        ]),
    ];

    let system = Paragraph::new(lines);
    frame.render_widget(system, area);
}

/// Render compact status line at bottom of screen
pub fn render_status_line(frame: &mut Frame, area: Rect, stats: &HardwareStats) {
    let status_text = stats.to_status_line();

    let style = if stats.has_critical_usage() {
        Style::default().bg(Color::Red).fg(Color::White)
    } else {
        Style::default().bg(Color::Black).fg(Color::Gray)
    };

    let status = Paragraph::new(status_text)
        .style(style)
        .alignment(Alignment::Center);

    frame.render_widget(status, area);
}

/// Get color based on usage percentage
fn get_usage_color(percent: f32) -> Color {
    if percent > 90.0 {
        Color::Red
    } else if percent > 70.0 {
        Color::Yellow
    } else {
        Color::Green
    }
}