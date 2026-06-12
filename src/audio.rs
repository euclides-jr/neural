use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{InterfaceType, Sample, FromSample, SizedSample};
use nam_rs::Model;
use ringbuf::{HeapProd, HeapCons, traits::{Producer, Consumer}};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use crate::BUFFER_SIZE;

const DELAY_BUF_SIZE: usize = 96000; // 2 seconds of delay memory at 48 kHz

// ── Device selection ──────────────────────────────────────────────────────────

pub fn find_usb_device(host: &cpal::Host) -> Option<cpal::Device> {
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

type RingProd = HeapProd<f32>;

pub fn build_input_stream(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    format: cpal::SampleFormat,
    channels: usize,
    mut prod: RingProd,
    input_level: Arc<AtomicU32>,
) -> Result<cpal::Stream, Box<dyn std::error::Error>> {
    let mut scratch: Vec<f32> = Vec::with_capacity(BUFFER_SIZE as usize * 8);

    macro_rules! build {
        ($ty:ty) => {
            device.build_input_stream(
                config.clone(),
                move |data: &[$ty], _: &cpal::InputCallbackInfo| {
                    scratch.clear();
                    scratch.extend(
                        data.chunks_exact(channels)
                            .map(|frame| frame[0].to_sample::<f32>()),
                    );

                    // Compute peak amplitude for level meter
                    let mut max_val = 0.0f32;
                    for &sample in &scratch {
                        let abs_val = sample.abs();
                        if abs_val > max_val {
                            max_val = abs_val;
                        }
                    }
                    input_level.fetch_max(max_val.to_bits(), Ordering::Relaxed);

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

type RingCons = HeapCons<f32>;

pub fn build_output_stream(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    format: cpal::SampleFormat,
    channels: usize,
    active_model: Arc<Mutex<Option<Model>>>,
    cons: RingCons,
    output_level: Arc<AtomicU32>,
    bypass: Arc<AtomicBool>,
    gate_enabled: Arc<AtomicBool>,
    gate_threshold_db: Arc<AtomicU32>,
    delay_enabled: Arc<AtomicBool>,
    delay_time_ms: Arc<AtomicU32>,
    delay_feedback: Arc<AtomicU32>,
    delay_mix: Arc<AtomicU32>,
) -> Result<cpal::Stream, Box<dyn std::error::Error>> {
    fn build_stream<T>(
        device: &cpal::Device,
        config: &cpal::StreamConfig,
        channels: usize,
        active_model: Arc<Mutex<Option<Model>>>,
        mut cons: RingCons,
        output_level: Arc<AtomicU32>,
        bypass: Arc<AtomicBool>,
        gate_enabled: Arc<AtomicBool>,
        gate_threshold_db: Arc<AtomicU32>,
        delay_enabled: Arc<AtomicBool>,
        delay_time_ms: Arc<AtomicU32>,
        delay_feedback: Arc<AtomicU32>,
        delay_mix: Arc<AtomicU32>,
        err_fn: impl Fn(cpal::Error) + Send + 'static,
    ) -> Result<cpal::Stream, Box<dyn std::error::Error>>
    where
        T: SizedSample + FromSample<f32> + Send + 'static,
    {
        let mut mono: Vec<f32> = vec![0.0; BUFFER_SIZE as usize * 8];
        let mut gate_gain = 0.0f32;
        let sr = config.sample_rate as f32;

        // Preallocate static delay line buffers (to be moved into callback closure)
        let mut delay_buffer = vec![0.0f32; DELAY_BUF_SIZE];
        let mut delay_write_idx = 0;

        let stream = device.build_output_stream(
            config.clone(),
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                let num_frames = data.len() / channels;

                if num_frames > mono.len() {
                    mono.resize(num_frames, 0.0);
                }
                let mono_slice = &mut mono[..num_frames];

                // 1. Batch-pop this period's samples from the capture ring.
                let got = cons.pop_slice(mono_slice);
                mono_slice[got..num_frames].fill(0.0);

                // Apply Noise Gate before NAM if enabled
                if gate_enabled.load(Ordering::Relaxed) {
                    let thresh_db = f32::from_bits(gate_threshold_db.load(Ordering::Relaxed));
                    let thresh_linear = 10.0f32.powf(thresh_db / 20.0);
                    let attack_ms = 2.0f32;
                    let release_ms = 150.0f32;
                    
                    let attack_coef = 1.0 - (-1.0 / (attack_ms * 0.001 * sr)).exp();
                    let release_coef = 1.0 - (-1.0 / (release_ms * 0.001 * sr)).exp();

                    for sample in mono_slice.iter_mut() {
                        let target_gain = if sample.abs() > thresh_linear { 1.0 } else { 0.0 };
                        if target_gain > gate_gain {
                            gate_gain += (target_gain - gate_gain) * attack_coef;
                        } else {
                            gate_gain += (target_gain - gate_gain) * release_coef;
                        }
                        *sample *= gate_gain;
                    }
                } else {
                    gate_gain = 1.0;
                }

                // 2. Run NAM on the whole buffer at once if not bypassed.
                if !bypass.load(Ordering::Relaxed) {
                    if let Ok(mut amp_lock) = active_model.try_lock() {
                        if let Some(ref mut model) = *amp_lock {
                            model.process_buffer(mono_slice);
                        }
                    }
                }

                // 2b. Apply Delay Effect after NAM model (guitar style delay chain)
                if delay_enabled.load(Ordering::Relaxed) {
                    let time_ms = delay_time_ms.load(Ordering::Relaxed) as f32;
                    let feedback = f32::from_bits(delay_feedback.load(Ordering::Relaxed));
                    let mix = f32::from_bits(delay_mix.load(Ordering::Relaxed));
                    
                    let delay_samples = ((time_ms * 0.001) * sr).round() as usize;

                    for sample in mono_slice.iter_mut() {
                        let read_idx = if delay_write_idx >= delay_samples {
                            delay_write_idx - delay_samples
                        } else {
                            delay_write_idx + DELAY_BUF_SIZE - delay_samples
                        };
                        
                        let wet = delay_buffer[read_idx];
                        let dry = *sample;
                        
                        // Feedback current dry signal + wet history back into the loop
                        delay_buffer[delay_write_idx] = dry + wet * feedback;
                        
                        // Mix final sample (combining dry and wet parts)
                        *sample = dry * (1.0 - mix) + wet * mix;
                        
                        delay_write_idx = (delay_write_idx + 1) % DELAY_BUF_SIZE;
                    }
                }

                // Compute peak amplitude for level meter
                let mut max_val = 0.0f32;
                for &sample in mono_slice.iter() {
                    let abs_val = sample.abs();
                    if abs_val > max_val {
                        max_val = abs_val;
                    }
                }
                output_level.fetch_max(max_val.to_bits(), Ordering::Relaxed);

                // 3. Write to every output channel.
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
        cpal::SampleFormat::I8  => build_stream::<i8> (device, config, channels, Arc::clone(&active_model), cons, output_level, bypass, Arc::clone(&gate_enabled), Arc::clone(&gate_threshold_db), Arc::clone(&delay_enabled), Arc::clone(&delay_time_ms), Arc::clone(&delay_feedback), Arc::clone(&delay_mix), err_fn)?,
        cpal::SampleFormat::I16 => build_stream::<i16>(device, config, channels, Arc::clone(&active_model), cons, output_level, bypass, Arc::clone(&gate_enabled), Arc::clone(&gate_threshold_db), Arc::clone(&delay_enabled), Arc::clone(&delay_time_ms), Arc::clone(&delay_feedback), Arc::clone(&delay_mix), err_fn)?,
        cpal::SampleFormat::I32 => build_stream::<i32>(device, config, channels, Arc::clone(&active_model), cons, output_level, bypass, Arc::clone(&gate_enabled), Arc::clone(&gate_threshold_db), Arc::clone(&delay_enabled), Arc::clone(&delay_time_ms), Arc::clone(&delay_feedback), Arc::clone(&delay_mix), err_fn)?,
        cpal::SampleFormat::I64 => build_stream::<i64>(device, config, channels, Arc::clone(&active_model), cons, output_level, bypass, Arc::clone(&gate_enabled), Arc::clone(&gate_threshold_db), Arc::clone(&delay_enabled), Arc::clone(&delay_time_ms), Arc::clone(&delay_feedback), Arc::clone(&delay_mix), err_fn)?,
        cpal::SampleFormat::U8  => build_stream::<u8> (device, config, channels, Arc::clone(&active_model), cons, output_level, bypass, Arc::clone(&gate_enabled), Arc::clone(&gate_threshold_db), Arc::clone(&delay_enabled), Arc::clone(&delay_time_ms), Arc::clone(&delay_feedback), Arc::clone(&delay_mix), err_fn)?,
        cpal::SampleFormat::U16 => build_stream::<u16>(device, config, channels, Arc::clone(&active_model), cons, output_level, bypass, Arc::clone(&gate_enabled), Arc::clone(&gate_threshold_db), Arc::clone(&delay_enabled), Arc::clone(&delay_time_ms), Arc::clone(&delay_feedback), Arc::clone(&delay_mix), err_fn)?,
        cpal::SampleFormat::U32 => build_stream::<u32>(device, config, channels, Arc::clone(&active_model), cons, output_level, bypass, Arc::clone(&gate_enabled), Arc::clone(&gate_threshold_db), Arc::clone(&delay_enabled), Arc::clone(&delay_time_ms), Arc::clone(&delay_feedback), Arc::clone(&delay_mix), err_fn)?,
        cpal::SampleFormat::U64 => build_stream::<u64>(device, config, channels, Arc::clone(&active_model), cons, output_level, bypass, Arc::clone(&gate_enabled), Arc::clone(&gate_threshold_db), Arc::clone(&delay_enabled), Arc::clone(&delay_time_ms), Arc::clone(&delay_feedback), Arc::clone(&delay_mix), err_fn)?,
        cpal::SampleFormat::F32 => build_stream::<f32>(device, config, channels, Arc::clone(&active_model), cons, output_level, bypass, Arc::clone(&gate_enabled), Arc::clone(&gate_threshold_db), Arc::clone(&delay_enabled), Arc::clone(&delay_time_ms), Arc::clone(&delay_feedback), Arc::clone(&delay_mix), err_fn)?,
        cpal::SampleFormat::F64 => build_stream::<f64>(device, config, channels, Arc::clone(&active_model), cons, output_level, bypass, Arc::clone(&gate_enabled), Arc::clone(&gate_threshold_db), Arc::clone(&delay_enabled), Arc::clone(&delay_time_ms), Arc::clone(&delay_feedback), Arc::clone(&delay_mix), err_fn)?,
        _ => return Err(format!("Unsupported output format: {format:?}").into()),
    })
}

// ── Real-time scheduling ──────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
pub fn set_realtime_priority() -> String {
    if try_rtkit_realtime() {
        "Enabled via RTKit (SCHED_FIFO priority 20)".to_string()
    } else if try_pthread_realtime() {
        "Enabled via pthread (SCHED_FIFO priority 20)".to_string()
    } else {
        "Disabled (neither RTKit nor pthread worked)".to_string()
    }
}

#[cfg(not(target_os = "linux"))]
pub fn set_realtime_priority() -> String {
    "Disabled (non-Linux platform)".to_string()
}

#[cfg(target_os = "linux")]
fn try_rtkit_realtime() -> bool {
    use dbus::blocking::Connection;
    use std::time::Duration;

    const RTKIT_PRIORITY: u32 = 20;

    let conn = match Connection::new_system() {
        Ok(c) => c,
        Err(_) => return false,
    };

    #[allow(unsafe_code)]
    unsafe {
        let limit = libc::rlimit {
            rlim_cur: 200_000,   // 200 ms in µs
            rlim_max: libc::RLIM_INFINITY,
        };
        libc::setrlimit(libc::RLIMIT_RTTIME, &limit);
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

    result.is_ok()
}

#[cfg(target_os = "linux")]
fn try_pthread_realtime() -> bool {
    use libc::{pthread_self, pthread_setschedparam, sched_param, SCHED_FIFO};
    const RT_PRIORITY: libc::c_int = 20;

    let param = sched_param { sched_priority: RT_PRIORITY };
    let ret = unsafe { pthread_setschedparam(pthread_self(), SCHED_FIFO, &param) };

    ret == 0
}

#[cfg(target_os = "linux")]
pub fn set_cpu_affinity() -> String {
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
                "Bound to big cores (4-7)".to_string()
            } else {
                format!("Failed: {}", std::io::Error::last_os_error())
            }
        } else {
            format!("Skipped (CPU count: {})", num_cpus)
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub fn set_cpu_affinity() -> String {
    "Skipped (non-Linux platform)".to_string()
}
