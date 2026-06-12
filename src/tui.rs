use nam_rs::Model;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Clear},
};

#[derive(Clone)]
pub struct BrowserItem {
    pub name: String,
    pub is_dir: bool,
    pub path: PathBuf,
}

pub struct TuiApp {
    pub device_name: String,
    pub channels_in: usize,
    pub channels_out: usize,
    pub sample_rate: u32,
    pub buffer_size: u32,
    pub one_way_latency_ms: f64,
    pub round_trip_latency_ms: f64,
    pub realtime_status: String,
    pub cpu_affinity_status: String,
    
    pub model_name: String,
    pub model_loaded: bool,
    pub model_expected_sr: Option<f64>,
    
    pub input_level: Arc<AtomicU32>,
    pub output_level: Arc<AtomicU32>,
    pub bypass: Arc<AtomicBool>,
    pub gate_enabled: Arc<AtomicBool>,
    pub gate_threshold_db: Arc<AtomicU32>,
    
    // Delay effect states
    pub delay_enabled: Arc<AtomicBool>,
    pub delay_time_ms: Arc<AtomicU32>,
    pub delay_feedback: Arc<AtomicU32>,
    pub delay_mix: Arc<AtomicU32>,
    
    // File system picker state
    pub picker_mode: bool,
    pub browser_dir: PathBuf,
    pub browser_items: Vec<BrowserItem>,
    pub browser_selected: usize,
    
    // Effects modal state
    pub effects_mode: bool,
    pub effects_selected: usize,
    
    pub active_model: Arc<Mutex<Option<Model>>>,
    pub logs: Vec<String>,
}

pub fn read_directory(path: &Path) -> std::io::Result<Vec<BrowserItem>> {
    let mut items = Vec::new();
    
    if let Some(parent) = path.parent() {
        items.push(BrowserItem {
            name: "📁 .. (Parent Directory)".to_string(),
            is_dir: true,
            path: parent.to_path_buf(),
        });
    }
    
    let mut dir_items = Vec::new();
    let mut file_items = Vec::new();
    
    for entry in std::fs::read_dir(path)? {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let entry_path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        
        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        
        let is_dir = metadata.is_dir();
        
        if is_dir {
            dir_items.push(BrowserItem {
                name: format!("📁 {}", name),
                is_dir: true,
                path: entry_path,
            });
        } else if name.ends_with(".nam") {
            file_items.push(BrowserItem {
                name: format!("🎵 {}", name),
                is_dir: false,
                path: entry_path,
            });
        }
    }
    
    dir_items.sort_by(|a, b| a.name.cmp(&b.name));
    file_items.sort_by(|a, b| a.name.cmp(&b.name));
    
    items.extend(dir_items);
    items.extend(file_items);
    
    Ok(items)
}

pub fn draw_tui(f: &mut ratatui::Frame, app: &TuiApp) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Header
            Constraint::Length(9), // Info panels
            Constraint::Length(5), // VU Meters
            Constraint::Min(5),    // Logs panel
        ])
        .split(f.area());

    // 1. Header block
    let bypass_status = if app.bypass.load(Ordering::Relaxed) {
        Span::styled(" BYPASSED ", Style::default().bg(Color::Yellow).fg(Color::Black).bold())
    } else {
        Span::styled(" ACTIVE ", Style::default().bg(Color::Green).fg(Color::Black).bold())
    };

    let header_text = vec![
        Line::from(vec![
            Span::styled(" ⚡ Neural Amp Modeler (NAM) Real-time Engine ", Style::default().fg(Color::Cyan).bold()),
            Span::raw(" | "),
            Span::raw("Engine Mode: "),
            bypass_status,
            Span::raw(" | "),
            Span::styled("[Space]", Style::default().fg(Color::Yellow).bold()),
            Span::raw(" Bypass | "),
            Span::styled("[M]", Style::default().fg(Color::Yellow).bold()),
            Span::raw(" Load Model | "),
            Span::styled("[E]", Style::default().fg(Color::Yellow).bold()),
            Span::raw(" Effects | "),
            Span::styled("[G]", Style::default().fg(Color::Yellow).bold()),
            Span::raw(" Gate | "),
            Span::styled("[Up/Dn]", Style::default().fg(Color::Yellow).bold()),
            Span::raw(" Thresh | "),
            Span::styled("[Q]", Style::default().fg(Color::Red).bold()),
            Span::raw(" Quit"),
        ])
    ];
    let header = Paragraph::new(header_text)
        .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Blue)));
    f.render_widget(header, chunks[0]);

    // 2. Info panels (Left & Right)
    let info_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(50),
            Constraint::Percentage(50),
        ])
        .split(chunks[1]);

    // Audio Engine Info
    let audio_info = vec![
        Line::from(vec![Span::styled("Device:      ", Style::default().bold().fg(Color::Green)), Span::raw(&app.device_name)]),
        Line::from(vec![Span::styled("Channels:    ", Style::default().bold().fg(Color::Green)), Span::raw(format!("{} In / {} Out", app.channels_in, app.channels_out))]),
        Line::from(vec![Span::styled("Sample Rate: ", Style::default().bold().fg(Color::Green)), Span::raw(format!("{} Hz", app.sample_rate))]),
        Line::from(vec![Span::styled("Buffer Size: ", Style::default().bold().fg(Color::Green)), Span::raw(format!("{} samples", app.buffer_size))]),
        Line::from(vec![
            Span::styled("Latency:     ", Style::default().bold().fg(Color::Green)),
            Span::raw(format!("{:.1} ms (one-way) / {:.1} ms (round-trip)", app.one_way_latency_ms, app.round_trip_latency_ms))
        ]),
        Line::from(vec![Span::styled("Real-time:   ", Style::default().bold().fg(Color::Green)), Span::raw(&app.realtime_status)]),
        Line::from(vec![Span::styled("CPU Cores:   ", Style::default().bold().fg(Color::Green)), Span::raw(&app.cpu_affinity_status)]),
    ];
    let audio_block = Paragraph::new(audio_info)
        .block(Block::default().borders(Borders::ALL).title(" Audio Engine Status ").border_style(Style::default().fg(Color::Green)));
    f.render_widget(audio_block, info_chunks[0]);

    // DSP / Model Info
    let model_status_span = if app.model_loaded {
        Span::styled("LOADED", Style::default().fg(Color::Green).bold())
    } else {
        Span::styled("NOT LOADED (Bypassed)", Style::default().fg(Color::Red).bold())
    };

    let mut model_info = vec![
        Line::from(vec![Span::styled("Model:       ", Style::default().bold().fg(Color::Cyan)), Span::raw(&app.model_name)]),
        Line::from(vec![Span::styled("Status:      ", Style::default().bold().fg(Color::Cyan)), model_status_span]),
    ];

    if app.model_loaded {
        if let Some(expected_sr) = app.model_expected_sr {
            model_info.push(Line::from(vec![
                Span::styled("Expected SR: ", Style::default().bold().fg(Color::Cyan)),
                Span::raw(format!("{:.0} Hz", expected_sr)),
            ]));

            if (expected_sr - app.sample_rate as f64).abs() > 1.0 {
                model_info.push(Line::from(vec![
                    Span::styled("⚠️ SR MISMATCH: Model expected rate differs from device rate!", Style::default().fg(Color::Yellow).bold())
                ]));
            } else {
                model_info.push(Line::from(vec![
                    Span::styled("✓ Sample rate matches", Style::default().fg(Color::Green))
                ]));
            }
        }
    } else {
        model_info.push(Line::from(vec![
            Span::styled("Press [M] to open picker and load a model.", Style::default().fg(Color::DarkGray))
        ]));
    }

    model_info.push(Line::from(vec![])); // Spacer

    let gate_active = app.gate_enabled.load(Ordering::Relaxed);
    let gate_status_span = if gate_active {
        Span::styled("ENABLED", Style::default().fg(Color::Green).bold())
    } else {
        Span::styled("DISABLED", Style::default().fg(Color::DarkGray))
    };
    let thresh_db = f32::from_bits(app.gate_threshold_db.load(Ordering::Relaxed));

    model_info.push(Line::from(vec![
        Span::styled("Noise Gate:  ", Style::default().bold().fg(Color::Cyan)),
        gate_status_span,
    ]));
    model_info.push(Line::from(vec![
        Span::styled("Threshold:   ", Style::default().bold().fg(Color::Cyan)),
        Span::raw(format!("{:.1} dBFS", thresh_db)),
    ]));

    let delay_active = app.delay_enabled.load(Ordering::Relaxed);
    let delay_status_span = if delay_active {
        let time = app.delay_time_ms.load(Ordering::Relaxed);
        let fb = f32::from_bits(app.delay_feedback.load(Ordering::Relaxed));
        Span::styled(format!("ENABLED ({}ms, {:.0}% FB)", time, fb * 100.0), Style::default().fg(Color::Green).bold())
    } else {
        Span::styled("DISABLED", Style::default().fg(Color::DarkGray))
    };

    model_info.push(Line::from(vec![
        Span::styled("Delay Effect:", Style::default().bold().fg(Color::Cyan)),
        Span::raw(" "),
        delay_status_span,
    ]));

    let model_block = Paragraph::new(model_info)
        .block(Block::default().borders(Borders::ALL).title(" DSP / NAM Model ").border_style(Style::default().fg(Color::Cyan)));
    f.render_widget(model_block, info_chunks[1]);

    // 3. VU Meters
    let input_bits = app.input_level.swap(0.0f32.to_bits(), Ordering::Relaxed);
    let input_peak = f32::from_bits(input_bits);
    
    let output_bits = app.output_level.swap(0.0f32.to_bits(), Ordering::Relaxed);
    let output_peak = f32::from_bits(output_bits);

    let input_db = amplitude_to_db(input_peak);
    let output_db = amplitude_to_db(output_peak);

    let meter_width = (chunks[2].width as i32 - 25).max(10) as usize;

    let mut input_spans = vec![
        Span::styled("Input Level:  ", Style::default().bold().fg(Color::Magenta)),
    ];
    input_spans.extend(make_vu_meter_spans(input_db, meter_width));

    let mut output_spans = vec![
        Span::styled("Output Level: ", Style::default().bold().fg(Color::Magenta)),
    ];
    output_spans.extend(make_vu_meter_spans(output_db, meter_width));

    let vu_lines = vec![
        Line::from(input_spans),
        Line::from(vec![]),
        Line::from(output_spans),
    ];

    let vu_block = Paragraph::new(vu_lines)
        .block(Block::default().borders(Borders::ALL).title(" Live Level Meters (dBFS) ").border_style(Style::default().fg(Color::Magenta)));
    f.render_widget(vu_block, chunks[2]);

    // 4. Logs panel
    let log_height = chunks[3].height as usize;
    let max_logs = if log_height > 2 { log_height - 2 } else { 0 };
    
    let start_idx = if app.logs.len() > max_logs {
        app.logs.len() - max_logs
    } else {
        0
    };

    let log_items: Vec<ListItem> = app.logs[start_idx..]
        .iter()
        .map(|log| ListItem::new(log.as_str()))
        .collect();

    let logs_list = List::new(log_items)
        .block(Block::default().borders(Borders::ALL).title(" System Logs / Events ").border_style(Style::default().fg(Color::DarkGray)))
        .style(Style::default().fg(Color::Gray));
    f.render_widget(logs_list, chunks[3]);

    // Render overlays on top
    if app.picker_mode {
        draw_picker_popup(f, app);
    } else if app.effects_mode {
        draw_effects_popup(f, app);
    }
}

fn draw_picker_popup(f: &mut ratatui::Frame, app: &TuiApp) {
    let area = f.area();
    
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(15),
            Constraint::Percentage(70),
            Constraint::Percentage(15),
        ])
        .split(area);
        
    let picker_area = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(10),
            Constraint::Percentage(80),
            Constraint::Percentage(10),
        ])
        .split(popup_layout[1])[1];
        
    f.render_widget(Clear, picker_area);
    
    let picker_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(picker_area);
        
    let items_list: Vec<ListItem> = app.browser_items.iter().enumerate().map(|(i, item)| {
        let style = if i == app.browser_selected {
            Style::default().fg(Color::Black).bg(Color::Cyan).bold()
        } else if item.is_dir {
            Style::default().fg(Color::Yellow).bold()
        } else {
            Style::default().fg(Color::White)
        };
        ListItem::new(item.name.as_str()).style(style)
    }).collect();
    
    let dir_name = app.browser_dir.to_string_lossy();
    let title = format!(" Model Picker (Browse .nam files) - {} ", dir_name);
    
    let list = List::new(items_list)
        .block(Block::default()
            .borders(Borders::ALL)
            .title(Span::styled(title, Style::default().fg(Color::Cyan).bold()))
            .border_style(Style::default().fg(Color::Cyan)))
        .style(Style::default().fg(Color::White));
        
    f.render_widget(list, picker_chunks[0]);
    
    let legend = Paragraph::new(" [Enter] Select/Open  |  [Up/Dn / J/K] Navigate  |  [Esc / M] Cancel ")
        .alignment(ratatui::layout::Alignment::Center)
        .style(Style::default().fg(Color::Yellow).bold());
        
    f.render_widget(legend, picker_chunks[1]);
}

fn draw_effects_popup(f: &mut ratatui::Frame, app: &TuiApp) {
    let area = f.area();
    
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(20),
            Constraint::Percentage(60),
            Constraint::Percentage(20),
        ])
        .split(area);
        
    let picker_area = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(15),
            Constraint::Percentage(70),
            Constraint::Percentage(15),
        ])
        .split(popup_layout[1])[1];
        
    f.render_widget(Clear, picker_area);
    
    let picker_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(picker_area);
        
    let delay_active = app.delay_enabled.load(Ordering::Relaxed);
    let delay_time = app.delay_time_ms.load(Ordering::Relaxed);
    let delay_fb = f32::from_bits(app.delay_feedback.load(Ordering::Relaxed));
    let delay_mx = f32::from_bits(app.delay_mix.load(Ordering::Relaxed));
    
    let mut items = Vec::new();
    
    let enable_str = if delay_active { "ENABLED (Active)" } else { "DISABLED (Bypassed)" };
    items.push((0, format!("Delay Effect:  {}", enable_str)));
    items.push((1, format!("Delay Time:    {} ms", delay_time)));
    items.push((2, format!("Feedback:      {:.0}%", delay_fb * 100.0)));
    items.push((3, format!("Wet Mix:       {:.0}%", delay_mx * 100.0)));
    
    let items_list: Vec<ListItem> = items.iter().map(|(idx, text)| {
        let style = if *idx == app.effects_selected {
            Style::default().fg(Color::Black).bg(Color::Magenta).bold()
        } else {
            Style::default().fg(Color::White)
        };
        ListItem::new(text.as_str()).style(style)
    }).collect();
    
    let list = List::new(items_list)
        .block(Block::default()
            .borders(Borders::ALL)
            .title(Span::styled(" Delay Effect Configuration ", Style::default().fg(Color::Magenta).bold()))
            .border_style(Style::default().fg(Color::Magenta)))
        .style(Style::default().fg(Color::White));
        
    f.render_widget(list, picker_chunks[0]);
    
    let legend = Paragraph::new(" [Up/Dn / J/K] Select Parameter  |  [Lf/Rt / H/L] Adjust  |  [Esc / E] Close ")
        .alignment(ratatui::layout::Alignment::Center)
        .style(Style::default().fg(Color::Yellow).bold());
        
    f.render_widget(legend, picker_chunks[1]);
}

fn amplitude_to_db(amp: f32) -> f32 {
    if amp <= 0.00001 {
        -100.0
    } else {
        20.0 * amp.log10()
    }
}

fn make_vu_meter_spans(db: f32, width: usize) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for i in 0..width {
        let step_db = -60.0 + (i as f32) * (60.0 / width as f32);
        let active = db >= step_db;
        let char_str = if active { "█" } else { "░" };
        let color = if !active {
            Color::DarkGray
        } else if step_db < -18.0 {
            Color::Green
        } else if step_db < -3.0 {
            Color::Yellow
        } else {
            Color::Red
        };
        spans.push(Span::styled(char_str, Style::default().fg(color)));
    }
    
    let db_text = if db <= -99.0 {
        " -∞ dB".to_string()
    } else {
        format!(" {:>5.1} dB", db)
    };
    spans.push(Span::styled(db_text, Style::default().fg(Color::White)));
    
    if db >= 0.0 {
        spans.push(Span::styled(" [CLIP]", Style::default().fg(Color::Red).bold()));
    }
    
    spans
}
