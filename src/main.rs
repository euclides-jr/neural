use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::InterfaceType;
use nam_rs::{NamModel, Model};
use std::sync::{Arc, Mutex};
use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if cfg!(debug_assertions) {
        println!("WARNING: Running in debug mode. Neural network inference is slow and will cause audio pops/cracks.");
        println!("Please run with: cargo run --release");
    }

    let host = cpal::default_host();

    println!("Searching for external audio interface (USB/iRig)...");
    
    let usb_device = find_usb_device(&host);

    let (input_device, output_device) = match usb_device {
        Some(device) => {
            let desc = device.description()?;
            println!("Selected device: {}", desc.name());
            (device.clone(), device)
        }
        None => {
            println!("No suitable external audio interface found. Engine will not start.");
            println!("\nAvailable devices for reference:");
            if let Ok(devices) = host.devices() {
                for d in devices {
                    if let Ok(desc) = d.description() {
                        println!("  - {} (Interface: {:?})", desc.name(), desc.interface_type());
                    }
                }
            }
            return Ok(());
        }
    };

    let mut input_config: cpal::StreamConfig = input_device.default_input_config()?.into();
    input_config.buffer_size = cpal::BufferSize::Fixed(256);
    let sample_rate = input_config.sample_rate;
    let channels = input_config.channels as usize;
    if channels == 0 {
        return Err("Audio interface has 0 channels".into());
    }
    println!("Audio config: {:?}", input_config);

    // Look for a .nam file
    let model_path = "model.nam";
    let amp: Option<Model> = if Path::new(model_path).exists() {
        println!("Loading NAM model from: {}", model_path);
        let nam_model = NamModel::from_file(model_path)?;
        let expected_sr = nam_model.expected_sample_rate();
        println!("Model expected sample rate: {} Hz", expected_sr);
        if (expected_sr - sample_rate as f64).abs() > 1.0 {
            println!("WARNING: Model expected sample rate ({} Hz) does not match audio device sample rate ({} Hz). This may cause incorrect processing speed/pitch.", expected_sr, sample_rate);
        }
        Some(Model::from_nam(&nam_model)?)
    } else {
        println!("Warning: 'model.nam' not found. Audio will be passed through without processing.");
        None
    };

    let amp_arc = Arc::new(Mutex::new(amp));
    let amp_output = Arc::clone(&amp_arc);

    let (tx, rx) = std::sync::mpsc::sync_channel::<f32>(sample_rate as usize * channels);

    // Pre-buffer 256 samples of silence to prevent initial underruns/starvation
    for _ in 0..256 {
        let _ = tx.send(0.0);
    }

    let input_data_fn = move |data: &[f32], _: &cpal::InputCallbackInfo| {
        for frame in data.chunks_exact(channels) {
            // Send only the first channel (mono guitar input) to the queue
            let _ = tx.send(frame[0]);
        }
    };

    let mut mono_buf = Vec::with_capacity(1024);

    let output_data_fn = move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
        let mut amp_lock = amp_output.lock().unwrap();
        
        let num_frames = data.len() / channels;
        mono_buf.resize(num_frames, 0.0);

        for sample in mono_buf.iter_mut() {
            *sample = rx.try_recv().unwrap_or(0.0);
        }

        if let Some(ref mut model) = *amp_lock {
            model.process_buffer(&mut mono_buf);
        }

        for (i, &sample) in mono_buf.iter().enumerate() {
            for c in 0..channels {
                data[i * channels + c] = sample;
            }
        }
    };

    println!("Starting audio streams on {}... Press Enter to stop.", input_device.description()?.name());

    let input_stream = input_device.build_input_stream(input_config.clone(), input_data_fn, err_fn, None)?;
    let output_stream = output_device.build_output_stream(input_config, output_data_fn, err_fn, None)?;

    input_stream.play()?;
    output_stream.play()?;

    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;

    Ok(())
}

fn find_usb_device(host: &cpal::Host) -> Option<cpal::Device> {
    host.devices().ok()?.find(|d| {
        if let Ok(desc) = d.description() {
            let name = desc.name().to_lowercase();
            // iRig HD X is reported as InterfaceType::Unknown on macOS
            let is_usb_type = desc.interface_type() == InterfaceType::Usb;
            let name_has_external = name.contains("usb") || name.contains("irig") || name.contains("audiobox") || name.contains("scarlett");
            
            (is_usb_type || name_has_external) && 
            desc.supports_input() && 
            desc.supports_output()
        } else {
            false
        }
    })
}

fn err_fn(err: cpal::Error) {
    eprintln!("An error occurred on stream: {}", err);
}
