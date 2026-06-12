use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use nam_rs::{NamModel, Model};
use ringbuf::{HeapRb, traits::{Split, Producer}};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use ratatui::{
    backend::CrosstermBackend,
    Terminal,
};
use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};

mod audio;
mod tui;

use tui::{TuiApp, draw_tui, read_directory};

pub const BUFFER_SIZE: u32 = 256;
pub const RING_CAPACITY: usize = BUFFER_SIZE as usize * 4;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut logs = Vec::new();

    if cfg!(debug_assertions) {
        logs.push("WARNING: Running in debug mode — NAM inference will be far too slow.".to_string());
        logs.push("Run with:  cargo run --release".to_string());
    }

    let realtime_status = audio::set_realtime_priority();
    logs.push(format!("Real-time priority scheduling: {}", realtime_status));

    let cpu_affinity_status = audio::set_cpu_affinity();
    logs.push(format!("CPU core affinity: {}", cpu_affinity_status));

    let host = cpal::default_host();
    logs.push("Searching for external audio interface (USB/iRig)...".to_string());

    let usb_device = audio::find_usb_device(&host);
    let (input_device, output_device) = match usb_device {
        Some(device) => {
            let desc = device.description()?;
            logs.push(format!("Selected device: {}", desc.name()));
            (device.clone(), device)
        }
        None => {
            logs.push("No suitable external audio interface found. Engine will not start.".to_string());
            logs.push("\nAvailable devices for reference:".to_string());
            if let Ok(devices) = host.devices() {
                for d in devices {
                    if let Ok(desc) = d.description() {
                        logs.push(format!("  - {} (Interface: {:?})", desc.name(), desc.interface_type()));
                    }
                }
            }
            // Print logs to console and exit
            for log in &logs {
                println!("{}", log);
            }
            return Ok(());
        }
    };

    // ── Stream configs ────────────────────────────────────────────────────────
    let input_supported_config = input_device.default_input_config()?;
    let input_format = input_supported_config.sample_format();
    let mut input_config: cpal::StreamConfig = input_supported_config.into();
    input_config.buffer_size = cpal::BufferSize::Fixed(BUFFER_SIZE);
    let sample_rate = input_config.sample_rate;
    let input_channels = input_config.channels as usize;
    if input_channels == 0 {
        return Err("Audio interface has 0 input channels".into());
    }

    let output_supported_config = output_device.default_output_config()?;
    let output_format = output_supported_config.sample_format();
    let mut output_config: cpal::StreamConfig = output_supported_config.into();
    output_config.sample_rate = sample_rate;
    output_config.buffer_size = cpal::BufferSize::Fixed(BUFFER_SIZE);
    let output_channels = output_config.channels as usize;
    if output_channels == 0 {
        return Err("Audio interface has 0 output channels".into());
    }

    let one_way_ms = BUFFER_SIZE as f64 / sample_rate as f64 * 1000.0;
    let round_trip_ms = one_way_ms * 2.0;
    logs.push(format!("Buffer size   : {} samples", BUFFER_SIZE));
    logs.push(format!("Sample rate   : {} Hz", sample_rate));
    logs.push(format!("One-way latency ≈ {:.1} ms  |  Round-trip ≈ {:.1} ms", one_way_ms, round_trip_ms));
    logs.push(format!("Input  format : {:?}", input_format));
    logs.push(format!("Output format : {:?}", output_format));

    // ── NAM model ─────────────────────────────────────────────────────────────
    let model_path = "model.nam";
    let mut model_loaded = false;
    let mut model_expected_sr = None;

    let amp: Option<Model> = if Path::new(model_path).exists() {
        logs.push(format!("Loading NAM model from: {}", model_path));
        match NamModel::from_file(model_path) {
            Ok(nam_model) => {
                let expected_sr = nam_model.expected_sample_rate();
                model_expected_sr = Some(expected_sr);
                logs.push(format!("Model expected sample rate: {} Hz", expected_sr));
                if (expected_sr - sample_rate as f64).abs() > 1.0 {
                    logs.push(format!(
                        "WARNING: Model expects {expected_sr} Hz but device is {sample_rate} Hz. \
                         Pitch/speed may be incorrect."
                    ));
                }
                match Model::from_nam(&nam_model) {
                    Ok(m) => {
                        model_loaded = true;
                        Some(m)
                    }
                    Err(e) => {
                        logs.push(format!("Error building model: {e}"));
                        None
                    }
                }
            }
            Err(e) => {
                logs.push(format!("Error loading model file: {e}"));
                None
            }
        }
    } else {
        logs.push("Warning: 'model.nam' not found — passing audio through without processing.".to_string());
        None
    };

    // Shared states for TUI real-time gauges, bypass, and noise gate
    let input_level = Arc::new(AtomicU32::new(0));
    let output_level = Arc::new(AtomicU32::new(0));
    let bypass = Arc::new(AtomicBool::new(false));
    let gate_enabled = Arc::new(AtomicBool::new(true));
    let gate_threshold_db = Arc::new(AtomicU32::new((-45.0f32).to_bits()));
    let active_model = Arc::new(Mutex::new(amp));

    // ── Lock-free capture ring ────────────────────────────────────────────────
    let (mut cap_prod, cap_cons) = HeapRb::<f32>::new(RING_CAPACITY).split();

    // Pre-fill with silence so the output callback has samples immediately
    for _ in 0..BUFFER_SIZE as usize {
        let _ = cap_prod.try_push(0.0f32);
    }

    // ── Build streams ─────────────────────────────────────────────────────────
    let device_name = input_device.description()?.name().to_string();
    logs.push(format!("Starting audio streams on {}...", device_name));

    let input_stream = audio::build_input_stream(
        &input_device, &input_config, input_format, input_channels,
        cap_prod, Arc::clone(&input_level),
    )?;

    let output_stream = audio::build_output_stream(
        &output_device, &output_config, output_format, output_channels,
        Arc::clone(&active_model),
        cap_cons,
        Arc::clone(&output_level),
        Arc::clone(&bypass),
        Arc::clone(&gate_enabled),
        Arc::clone(&gate_threshold_db),
    )?;

    input_stream.play()?;
    output_stream.play()?;
    logs.push("Audio streaming active.".to_string());

    // ── TUI SETUP ─────────────────────────────────────────────────────────────
    // Set panic hook to restore terminal state before panic output is printed
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = disable_raw_mode();
        let mut stdout = std::io::stdout();
        let _ = execute!(stdout, LeaveAlternateScreen);
        default_hook(panic_info);
    }));

    // Initialize picker filesystem path
    let browser_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let browser_items = read_directory(&browser_dir).unwrap_or_default();

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut tui_app = TuiApp {
        device_name,
        channels_in: input_channels,
        channels_out: output_channels,
        sample_rate,
        buffer_size: BUFFER_SIZE,
        one_way_latency_ms: one_way_ms,
        round_trip_latency_ms: round_trip_ms,
        realtime_status,
        cpu_affinity_status,
        model_name: if model_loaded { model_path.to_string() } else { "None (Bypassed)".to_string() },
        model_loaded,
        model_expected_sr,
        input_level,
        output_level,
        bypass,
        gate_enabled,
        gate_threshold_db,
        picker_mode: false,
        browser_dir,
        browser_items,
        browser_selected: 0,
        active_model,
        logs,
    };

    // TUI loop
    let mut last_tick = std::time::Instant::now();
    let tick_rate = std::time::Duration::from_millis(33); // ~30 FPS

    loop {
        terminal.draw(|f| {
            draw_tui(f, &tui_app);
        })?;

        let timeout = tick_rate
            .checked_sub(last_tick.elapsed())
            .unwrap_or(std::time::Duration::from_secs(0));

        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == event::KeyEventKind::Press {
                    if tui_app.picker_mode {
                        match key.code {
                            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('m') | KeyCode::Char('M') => {
                                tui_app.picker_mode = false;
                            }
                            KeyCode::Char('k') | KeyCode::Char('K') | KeyCode::Up => {
                                if tui_app.browser_selected > 0 {
                                    tui_app.browser_selected -= 1;
                                } else if !tui_app.browser_items.is_empty() {
                                    tui_app.browser_selected = tui_app.browser_items.len() - 1;
                                }
                            }
                            KeyCode::Char('j') | KeyCode::Char('J') | KeyCode::Down => {
                                if !tui_app.browser_items.is_empty() {
                                    tui_app.browser_selected = (tui_app.browser_selected + 1) % tui_app.browser_items.len();
                                }
                            }
                            KeyCode::Enter => {
                                if !tui_app.browser_items.is_empty() {
                                    let selected_item = tui_app.browser_items[tui_app.browser_selected].clone();
                                    if selected_item.is_dir {
                                        // Navigate into directory
                                        match read_directory(&selected_item.path) {
                                            Ok(items) => {
                                                tui_app.browser_dir = selected_item.path;
                                                tui_app.browser_items = items;
                                                tui_app.browser_selected = 0;
                                            }
                                            Err(e) => {
                                                tui_app.logs.push(format!("Error entering folder: {e}"));
                                            }
                                        }
                                    } else {
                                        // Load selected .nam model
                                        let path_str = selected_item.path.to_string_lossy().to_string();
                                        tui_app.logs.push(format!("Loading model: {}...", path_str));
                                        
                                        match NamModel::from_file(&selected_item.path) {
                                            Ok(nam_model) => {
                                                let expected_sr = nam_model.expected_sample_rate();
                                                match Model::from_nam(&nam_model) {
                                                    Ok(new_model) => {
                                                        if let Ok(mut amp_lock) = tui_app.active_model.lock() {
                                                            *amp_lock = Some(new_model);
                                                            tui_app.model_loaded = true;
                                                            tui_app.model_name = selected_item.path.file_name()
                                                                .unwrap_or_default()
                                                                .to_string_lossy()
                                                                .to_string();
                                                            tui_app.model_expected_sr = Some(expected_sr);
                                                            tui_app.logs.push(format!("Successfully loaded model: {}", tui_app.model_name));
                                                            tui_app.picker_mode = false;
                                                        }
                                                    }
                                                    Err(e) => {
                                                        tui_app.logs.push(format!("Error building model: {e}"));
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                tui_app.logs.push(format!("Error loading model file: {e}"));
                                            }
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    } else {
                        match key.code {
                            KeyCode::Char('q') | KeyCode::Esc => {
                                break;
                            }
                            KeyCode::Char(' ') => {
                                let prev = tui_app.bypass.load(Ordering::Relaxed);
                                tui_app.bypass.store(!prev, Ordering::Relaxed);
                                let status_str = if !prev { "Bypassed NAM processing" } else { "Enabled NAM processing" };
                                tui_app.logs.push(format!("User Action: {}", status_str));
                            }
                            KeyCode::Char('g') | KeyCode::Char('G') => {
                                let prev = tui_app.gate_enabled.load(Ordering::Relaxed);
                                tui_app.gate_enabled.store(!prev, Ordering::Relaxed);
                                let status_str = if !prev { "Enabled Noise Gate" } else { "Disabled Noise Gate" };
                                tui_app.logs.push(format!("User Action: {}", status_str));
                            }
                            KeyCode::Char('m') | KeyCode::Char('M') => {
                                tui_app.picker_mode = true;
                                tui_app.browser_selected = 0;
                                if let Ok(items) = read_directory(&tui_app.browser_dir) {
                                    tui_app.browser_items = items;
                                }
                            }
                            KeyCode::Up => {
                                let prev_db = f32::from_bits(tui_app.gate_threshold_db.load(Ordering::Relaxed));
                                let next_db = (prev_db + 1.0).min(0.0);
                                tui_app.gate_threshold_db.store(next_db.to_bits(), Ordering::Relaxed);
                                tui_app.logs.push(format!("Noise Gate: Threshold set to {:.1} dB", next_db));
                            }
                            KeyCode::Down => {
                                let prev_db = f32::from_bits(tui_app.gate_threshold_db.load(Ordering::Relaxed));
                                let next_db = (prev_db - 1.0).max(-100.0);
                                tui_app.gate_threshold_db.store(next_db.to_bits(), Ordering::Relaxed);
                                tui_app.logs.push(format!("Noise Gate: Threshold set to {:.1} dB", next_db));
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        if last_tick.elapsed() >= tick_rate {
            last_tick = std::time::Instant::now();
        }
    }

    // ── TUI CLEANUP ───────────────────────────────────────────────────────────
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;

    println!("Audio engine and streams stopped. Goodbye!");

    Ok(())
}
