use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat, StreamConfig};
use crossbeam_channel::Sender;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};

use crate::buffers::AudioBufferPool;

#[allow(dead_code)]
#[derive(Debug, Clone)]
/// Basic negotiated audio stream parameters.
pub struct AudioParams {
    pub sample_rate: u32,
    pub channels: u16,
    pub sample_format: SampleFormat,
}

#[allow(dead_code)]
/// Handle for an active input stream plus captured parameters.
pub struct InputStreamHandle {
    pub stream: cpal::Stream,
    pub params: AudioParams,
}

/// Enumerate available input and output devices.
pub fn list_devices() -> Result<(Vec<Device>, Vec<Device>)> {
    let host = cpal::default_host();
    let inputs: Vec<_> = host.input_devices().context("input_devices")?.collect();
    let outputs: Vec<_> = host.output_devices().context("output_devices")?.collect();
    Ok((inputs, outputs))
}

/// Best-effort device name (fallback to "<unknown>").
pub fn device_name(dev: &Device) -> String {
    dev.name().unwrap_or_else(|_| "<unknown>".into())
}

#[allow(dead_code)]
/// Build and start a CPAL input stream. Captured chunks are copied into a buffer
/// from the pool: first 4 bytes store payload length (LE) then raw sample bytes.
pub fn build_input_stream(
    dev: &Device,
    pool: Arc<AudioBufferPool>,
    send_ready: Sender<usize>,
    running: Arc<AtomicBool>,
) -> Result<InputStreamHandle> {
    let cfg = dev.default_input_config()?;
    let sample_format = cfg.sample_format();
    let config: StreamConfig = cfg.clone().into();
    let params = AudioParams { sample_rate: config.sample_rate.0, channels: config.channels, sample_format };
    let counter = Arc::new(AtomicU64::new(0));

    // Each callback -> one buffer. First 4 bytes length (LE). Remaining bytes = packed raw samples.
    let make_callback = |_bytes_per_sample: usize| {
        let pool = pool.clone(); let send_ready = send_ready.clone(); let running = running.clone(); let counter = counter.clone();
        move |raw: &[u8]| {
            if !running.load(Ordering::Relaxed) { return; }
            if let Some(idx) = pool.pop() {
                let mut guard = pool.data[idx].lock();
                let buf_slice: &mut [u8] = &mut *guard;
                if buf_slice.len() < 5 { return; }
                let max_payload = buf_slice.len()-4;
                let to_copy = raw.len().min(max_payload);
                // write length
                let len_le = (to_copy as u32).to_le_bytes();
                buf_slice[0..4].copy_from_slice(&len_le);
                unsafe { std::ptr::copy_nonoverlapping(raw.as_ptr(), buf_slice[4..].as_mut_ptr(), to_copy); }
                let _ = send_ready.send(idx);
                let n = counter.fetch_add(1, Ordering::Relaxed) + 1;
                if n % 100 == 0 { println!("[AUDIO] {} chunks", n); }
            } else {
                // drop if no free buffer
            }
        }
    };

    let stream = match sample_format {
        SampleFormat::F32 => {
            let cb = make_callback(4);
            dev.build_input_stream(&config, move |data: &[f32], _| {
                let raw = unsafe { std::slice::from_raw_parts(data.as_ptr() as *const u8, data.len()*4) };
                cb(raw);
            }, move |e| eprintln!("[AUDIO][ERR] {e}"), None)?
        }
        SampleFormat::I16 => {
            let cb = make_callback(2);
            dev.build_input_stream(&config, move |data: &[i16], _| {
                let raw = unsafe { std::slice::from_raw_parts(data.as_ptr() as *const u8, data.len()*2) };
                cb(raw);
            }, move |e| eprintln!("[AUDIO][ERR] {e}"), None)?
        }
        SampleFormat::U16 => {
            let cb = make_callback(2);
            dev.build_input_stream(&config, move |data: &[u16], _| {
                let raw = unsafe { std::slice::from_raw_parts(data.as_ptr() as *const u8, data.len()*2) };
                cb(raw);
            }, move |e| eprintln!("[AUDIO][ERR] {e}"), None)?
        }
        other => {
            println!(
                "[AUDIO] Unsupported sample format {:?}, falling back via f32 conversion",
                other
            );
            let cb = make_callback(4);
            dev.build_input_stream(&config, move |data: &[f32], _| {
                let raw = unsafe { std::slice::from_raw_parts(data.as_ptr() as *const u8, data.len()*4) };
                cb(raw);
            }, move |e| eprintln!("[AUDIO][ERR] {e}"), None)?
        }
    };
    stream.play()?;
    println!(
        "[AUDIO] Input stream running: {} Hz, {} ch, {:?}",
        params.sample_rate, params.channels, params.sample_format
    );
    Ok(InputStreamHandle { stream, params })
}

#[allow(dead_code)]
/// Handle for an active output stream.
pub struct OutputStreamHandle {
    pub stream: cpal::Stream,
}

#[allow(dead_code)]
/// Build a simple f32 output stream that copies raw f32 bytes from channel.
pub fn build_output_stream(
    dev: &Device,
    _params: &AudioParams,
    rx_audio: crossbeam_channel::Receiver<Vec<u8>>,
    running: Arc<AtomicBool>,
) -> Result<OutputStreamHandle> {
    // For now use default output config; future work: match server params.
    let cfg = dev.default_output_config()?;
    let config: StreamConfig = cfg.clone().into();
    let stream = dev.build_output_stream(
        &config,
        move |out: &mut [f32], _| {
            if !running.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            if let Ok(buf) = rx_audio.try_recv() {
                // naive copy, ignoring format differences
                let frames = out.len().min(buf.len() / 4);
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        buf.as_ptr(),
                        out.as_mut_ptr() as *mut u8,
                        frames * 4,
                    );
                }
            }
        },
        move |err| {
            eprintln!("Output stream error: {err}");
        },
        None,
    )?;
    stream.play()?;
    Ok(OutputStreamHandle { stream })
}
