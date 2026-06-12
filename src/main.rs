use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{InterfaceType, Sample, FromSample, SizedSample};
use nam_rs::{NamModel, Model};
use ringbuf::{HeapRb, HeapProd, HeapCons, traits::{Producer, Consumer, Split}};
use std::path::Path;
use libc;

// ── Latency tuning ────────────────────────────────────────────────────────────
//
// BUFFER_SIZE is the number of samples per audio callback period.
// Latency (one-way) ≈ BUFFER_SIZE / sample_rate.
// Round-trip latency (guitar → speaker) ≈ 2 × that.
//
//   BUFFER_SIZE │ 44.1 kHz │ 48 kHz  │ notes
//   ────────────┼──────────┼─────────┼───────────────────────────────────────
//           64  │  1.5 ms  │  1.3 ms │ Very aggressive; may crack on slow HW
//          128  │  2.9 ms  │  2.7 ms │ ★ Recommended starting point
//          256  │  5.8 ms  │  5.3 ms │ Safe fallback if 128 causes cracks
//          512  │ 11.6 ms  │ 10.7 ms │ Comfortable; noticeable but playable
//
// If you hear cracks/pops, double BUFFER_SIZE and try again.
const BUFFER_SIZE: u32 = 256;

// Ring buffer between the input and output callbacks.
// 4× the period gives enough headroom for brief scheduling jitter while
// keeping the ring small enough to stay hot in L1/L2 cache.
const RING_CAPACITY: usize = BUFFER_SIZE as usize * 4;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if cfg!(debug_assertions) {
        eprintln!("WARNING: Running in debug mode — NAM inference will be far too slow.");
        eprintln!("Run with:  cargo run --release");
    }

    // Elevate the process to real-time scheduling priority if the system allows
    // it (requires @audio rtprio limits or CAP_SYS_NICE capability).
    set_realtime_priority();
    set_cpu_affinity();

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
    println!("Buffer size   : {} samples", BUFFER_SIZE);
    println!("Sample rate   : {} Hz", sample_rate);
    println!("One-way latency ≈ {:.1} ms  |  Round-trip ≈ {:.1} ms",
        one_way_ms, one_way_ms * 2.0);
    println!("Input  format : {:?}", input_format);
    println!("Output format : {:?}", output_format);

    // ── NAM model ─────────────────────────────────────────────────────────────
    let model_path = "model.nam";
    let amp: Option<Model> = if Path::new(model_path).exists() {
        println!("Loading NAM model from: {}", model_path);
        let nam_model = NamModel::from_file(model_path)?;
        let expected_sr = nam_model.expected_sample_rate();
        println!("Model expected sample rate: {} Hz", expected_sr);
        if (expected_sr - sample_rate as f64).abs() > 1.0 {
            println!(
                "WARNING: Model expects {expected_sr} Hz but device is {sample_rate} Hz. \
                 Pitch/speed may be incorrect."
            );
        }
        Some(Model::from_nam(&nam_model)?)
    } else {
        println!("Warning: 'model.nam' not found — passing audio through without processing.");
        None
    };

    // ── Lock-free capture ring ────────────────────────────────────────────────
    // The input callback pushes raw f32 samples here.
    // The output callback pops them, runs NAM inline, and writes to the DAC.
    //
    // Running NAM *inside* the output callback keeps latency at a strict
    // 1-period input + 1-period output = 2 × BUFFER_SIZE / sample_rate.
    // There is no inter-thread handoff and no extra buffering delay.
    let (mut cap_prod, cap_cons) = HeapRb::<f32>::new(RING_CAPACITY).split();

    // Pre-fill with silence so the output callback has samples to read
    // before the input stream has delivered its first buffer.
    for _ in 0..BUFFER_SIZE as usize {
        let _ = cap_prod.try_push(0.0f32);
    }

    // ── Build streams ─────────────────────────────────────────────────────────
    println!(
        "Starting audio streams on {}... Press Enter to stop.",
        input_device.description()?.name()
    );

    let input_stream = build_input_stream(
        &input_device, &input_config, input_format, input_channels,
        cap_prod,
    )?;

    let output_stream = build_output_stream(
        &output_device, &output_config, output_format, output_channels,
        amp,
        cap_cons,
    )?;

    input_stream.play()?;
    output_stream.play()?;

    println!("Streaming. Press Enter to quit.");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;

    Ok(())
}

// ── Device selection ──────────────────────────────────────────────────────────

fn find_usb_device(host: &cpal::Host) -> Option<cpal::Device> {
    host.devices().ok()?.find(|d| {
        if let Ok(desc) = d.description() {
            let name = desc.name().to_lowercase();
            let is_usb_type = desc.interface_type() == InterfaceType::Usb;
            let name_has_external = name.contains("usb")
                || name.contains("irig")
                || name.contains("audiobox")
                || name.contains("scarlett");
            (is_usb_type || name_has_external)
                && desc.supports_input()
                && desc.supports_output()
        } else {
            false
        }
    })
}

// ── Input stream builder ──────────────────────────────────────────────────────
//
// Batch-converts channel 0 of each frame to f32 into a small stack buffer,
// then pushes the whole slice into the ring in one shot — avoiding per-sample
// closure dispatch overhead in the RT callback.

type RingProd = HeapProd<f32>;

fn build_input_stream(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    format: cpal::SampleFormat,
    channels: usize,
    mut prod: RingProd,
) -> Result<cpal::Stream, Box<dyn std::error::Error>> {
    // Scratch buffer allocated once on the heap; reused every callback.
    // Generously sized to avoid reallocation if period size varies.
    let mut scratch: Vec<f32> = Vec::with_capacity(BUFFER_SIZE as usize * 8);

    macro_rules! build {
        ($ty:ty) => {
            device.build_input_stream(
                config.clone(),
                move |data: &[$ty], _: &cpal::InputCallbackInfo| {
                    // Downmix to mono (channel 0 only — guitars are mono).
                    scratch.clear();
                    scratch.extend(
                        data.chunks_exact(channels)
                            .map(|frame| frame[0].to_sample::<f32>()),
                    );
                    // Push the whole period in one call — minimal ring contention.
                    prod.push_slice(&scratch);
                },
                |err| eprintln!("Input stream error: {err}"),
                None,
            )?
        };
    }

    Ok(match format {
        cpal::SampleFormat::I8  => build!(i8),
        cpal::SampleFormat::I16 => build!(i16),
        cpal::SampleFormat::I32 => build!(i32),
        cpal::SampleFormat::I64 => build!(i64),
        cpal::SampleFormat::U8  => build!(u8),
        cpal::SampleFormat::U16 => build!(u16),
        cpal::SampleFormat::U32 => build!(u32),
        cpal::SampleFormat::U64 => build!(u64),
        cpal::SampleFormat::F32 => build!(f32),
        cpal::SampleFormat::F64 => build!(f64),
        _ => return Err(format!("Unsupported input format: {format:?}").into()),
    })
}

// ── Output stream builder ─────────────────────────────────────────────────────
//
// NAM inference runs *inside* this callback for the lowest possible latency.
// Samples are batch-popped from the capture ring into the mono scratch buffer
// in one slice operation, then NAM processes the whole period at once.

type RingCons = HeapCons<f32>;

fn build_output_stream(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    format: cpal::SampleFormat,
    channels: usize,
    amp: Option<Model>,
    cons: RingCons,
) -> Result<cpal::Stream, Box<dyn std::error::Error>> {
    fn build_stream<T>(
        device: &cpal::Device,
        config: &cpal::StreamConfig,
        channels: usize,
        mut amp: Option<Model>,
        mut cons: RingCons,
        err_fn: impl Fn(cpal::Error) + Send + 'static,
    ) -> Result<cpal::Stream, Box<dyn std::error::Error>>
    where
        T: SizedSample + FromSample<f32> + Send + 'static,
    {
        // Scratch buffer reused each callback — generously sized capacity to avoid
        // reallocation inside the loop even if CPAL period size varies or increases.
        let mut mono: Vec<f32> = vec![0.0; BUFFER_SIZE as usize * 8];

        let stream = device.build_output_stream(
            config.clone(),
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                let num_frames = data.len() / channels;

                if num_frames > mono.len() {
                    // Fallback to resize if CPAL delivers an exceptionally large buffer,
                    // accepting the allocation penalty just this once to prevent panic.
                    mono.resize(num_frames, 0.0);
                }
                let mono_slice = &mut mono[..num_frames];

                // 1. Batch-pop this period's samples from the capture ring.
                //    pop_slice copies up to num_frames samples in one call,
                //    returning the count actually available (≤ num_frames).
                let got = cons.pop_slice(mono_slice);
                // Zero-pad any gap (underrun) to avoid stale data.
                mono_slice[got..num_frames].fill(0.0);

                // 2. Run NAM on the whole buffer at once (most efficient batch size).
                if let Some(ref mut model) = amp {
                    model.process_buffer(mono_slice);
                }

                // 3. Write to every output channel.
                //    chunks_exact_mut and zip allow compiler to auto-vectorize and unroll the loop.
                for (frame, &s) in data.chunks_exact_mut(channels).zip(mono_slice.iter()) {
                    let out = T::from_sample(s);
                    frame.fill(out);
                }
            },
            err_fn,
            None,
        )?;
        Ok(stream)
    }

    let err_fn = |err| eprintln!("Output stream error: {err}");
    Ok(match format {
        cpal::SampleFormat::I8  => build_stream::<i8> (device, config, channels, amp, cons, err_fn)?,
        cpal::SampleFormat::I16 => build_stream::<i16>(device, config, channels, amp, cons, err_fn)?,
        cpal::SampleFormat::I32 => build_stream::<i32>(device, config, channels, amp, cons, err_fn)?,
        cpal::SampleFormat::I64 => build_stream::<i64>(device, config, channels, amp, cons, err_fn)?,
        cpal::SampleFormat::U8  => build_stream::<u8> (device, config, channels, amp, cons, err_fn)?,
        cpal::SampleFormat::U16 => build_stream::<u16>(device, config, channels, amp, cons, err_fn)?,
        cpal::SampleFormat::U32 => build_stream::<u32>(device, config, channels, amp, cons, err_fn)?,
        cpal::SampleFormat::U64 => build_stream::<u64>(device, config, channels, amp, cons, err_fn)?,
        cpal::SampleFormat::F32 => build_stream::<f32>(device, config, channels, amp, cons, err_fn)?,
        cpal::SampleFormat::F64 => build_stream::<f64>(device, config, channels, amp, cons, err_fn)?,
        _ => return Err(format!("Unsupported output format: {format:?}").into()),
    })
}

// ── Real-time scheduling ──────────────────────────────────────────────────────
//
// On modern Linux with cgroup v2 + systemd (the common desktop setup), calling
// pthread_setschedparam(SCHED_FIFO) directly fails with EPERM even with the
// @audio rtprio limit set and even with CAP_SYS_NICE on the binary. This is
// because the kernel blocks RT scheduling for tasks inside a user.slice cgroup
// that has no cpu.rt_runtime_us — a cgroup v2 limitation.
//
// The correct solution (used by PipeWire, JACK, etc.) is to ask the RTKit
// D-Bus service to elevate the thread on our behalf. RTKit runs as root in the
// system slice and is allowed to set SCHED_FIFO.
//
// We try RTKit first, then fall back to a direct pthread call (works on
// non-systemd / cgroup v1 systems, or when the binary has CAP_SYS_NICE and
// the cgroup allows it).
#[cfg(target_os = "linux")]
fn set_realtime_priority() {
    if try_rtkit_realtime() {
        return;
    }
    try_pthread_realtime();
}

#[cfg(target_os = "linux")]
fn try_rtkit_realtime() -> bool {
    use dbus::blocking::Connection;
    use std::time::Duration;

    // RTKit caps priority at 20 for unprivileged callers.
    const RTKIT_PRIORITY: u32 = 20;

    let conn = match Connection::new_system() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("RTKit: could not connect to system D-Bus: {e}");
            return false;
        }
    };

    // RTKit requires two preconditions before it will honour MakeThreadRealtime:
    //
    // 1. RLIMIT_RTTIME must be finite (safety guard against runaway RT threads).
    //    Set it to 200 ms — same as PipeWire's default.
    // 2. The calling process's nice level must be ≤ MinNiceLevel (here: -15).
    //    Our @audio limits.conf grants nice -19, so setpriority succeeds.
    #[allow(unsafe_code)]
    unsafe {
        // Precondition 1: finite RT-time budget.
        let limit = libc::rlimit {
            rlim_cur: 200_000,   // 200 ms in µs
            rlim_max: libc::RLIM_INFINITY,
        };
        libc::setrlimit(libc::RLIMIT_RTTIME, &limit);

        // Precondition 2: nice level ≤ RTKit's MinNiceLevel (-15).
        // PRIO_PROCESS = 0, pid = 0 means "this process".
        libc::setpriority(libc::PRIO_PROCESS, 0, -15);
    }

    let tid = unsafe { libc::syscall(libc::SYS_gettid) } as u64;

    let proxy = conn.with_proxy(
        "org.freedesktop.RealtimeKit1",
        "/org/freedesktop/RealtimeKit1",
        Duration::from_secs(5),
    );

    let result: Result<(), dbus::Error> = proxy.method_call(
        "org.freedesktop.RealtimeKit1",
        "MakeThreadRealtime",
        (tid, RTKIT_PRIORITY),
    );

    match result {
        Ok(()) => {
            println!("✓ Real-time scheduling enabled via RTKit (SCHED_FIFO priority {RTKIT_PRIORITY})");
            true
        }
        Err(e) => {
            // RTKit 0.13 uses the deprecated polkit unix-process subject which
            // polkit >= 124 (Ubuntu 24.04+) no longer accepts, causing EPERM.
            // Fix: add a polkit JS rule granting the action to the @audio group:
            //
            //   sudo tee /etc/polkit-1/rules.d/10-rtkit-audio.rules << 'EOF'
            //   polkit.addRule(function(action, subject) {
            //     if ((action.id == "org.freedesktop.RealtimeKit1.acquire-real-time" ||
            //          action.id == "org.freedesktop.RealtimeKit1.acquire-high-priority") &&
            //         subject.isInGroup("audio")) { return polkit.Result.YES; }
            //   });
            //   EOF
            eprintln!("⚠ RTKit: {e}");
            eprintln!("  Fix (one-time, requires sudo):");
            eprintln!("  sudo tee /etc/polkit-1/rules.d/10-rtkit-audio.rules << 'EOF'");
            eprintln!("  polkit.addRule(function(action, subject) {{");
            eprintln!("    if ((action.id == \"org.freedesktop.RealtimeKit1.acquire-real-time\" ||");
            eprintln!("         action.id == \"org.freedesktop.RealtimeKit1.acquire-high-priority\") &&");
            eprintln!("        subject.isInGroup(\"audio\")) {{ return polkit.Result.YES; }}");
            eprintln!("  }});");
            eprintln!("  EOF");
            false
        }
    }
}

#[cfg(target_os = "linux")]
fn try_pthread_realtime() {
    use libc::{pthread_self, pthread_setschedparam, sched_param, SCHED_FIFO};
    const RT_PRIORITY: libc::c_int = 20;

    let param = sched_param { sched_priority: RT_PRIORITY };
    let ret = unsafe { pthread_setschedparam(pthread_self(), SCHED_FIFO, &param) };

    if ret == 0 {
        println!("✓ Real-time scheduling enabled via pthread (SCHED_FIFO priority {RT_PRIORITY})");
    } else {
        let err = std::io::Error::last_os_error();
        println!(
            "⚠ Could not set real-time scheduling: {err}\n  \
             This system uses cgroup v2; neither RTKit nor pthread worked.\n  \
             Try: sudo setcap cap_sys_nice+ep target/release/neural"
        );
    }
}

#[cfg(not(target_os = "linux"))]
fn set_realtime_priority() {
    // RT scheduling setup is Linux-specific; skip silently on other platforms.
}

#[cfg(target_os = "linux")]
fn set_cpu_affinity() {
    #[allow(unsafe_code)]
    unsafe {
        let num_cpus = libc::sysconf(libc::_SC_NPROCESSORS_ONLN);
        if num_cpus == 8 {
            let mut mask: libc::cpu_set_t = std::mem::zeroed();
            libc::CPU_ZERO(&mut mask);
            // Bind to big/performance cores 4, 5, 6, 7 on Radxa big.LITTLE architecture.
            libc::CPU_SET(4, &mut mask);
            libc::CPU_SET(5, &mut mask);
            libc::CPU_SET(6, &mut mask);
            libc::CPU_SET(7, &mut mask);
            let ret = libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mask);
            if ret == 0 {
                println!("✓ CPU affinity set to big cores (4-7)");
            } else {
                eprintln!("⚠ Could not set CPU affinity: {}", std::io::Error::last_os_error());
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn set_cpu_affinity() {
    // CPU affinity setup is Linux-specific; skip silently on other platforms.
}

