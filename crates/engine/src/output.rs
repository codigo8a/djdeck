//! Salida de audio por cpal (ALSA/PipeWire en Linux).
//!
//! Elección de formato, en este orden:
//! 1. El SR que pida `DJDECK_RATE` (útil para comparar).
//! 2. 48 kHz, que es el reloj del grafo de PipeWire en este equipo: si el
//!    cliente abre a 44.1 kHz, PipeWire remuestrea y el reloj del callback deja
//!    de coincidir con el reloj de pared.
//! 3. Lo que ofrezca el dispositivo por defecto.
//!
//! El tamaño de bloque pedido es 256 frames; si el backend lo rechaza se cae a
//! `BufferSize::Default` en vez de no arrancar.

use crate::{Controls, Mixer, PendingLoads};
use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, SampleFormat, Stream, StreamConfig, SupportedStreamConfigRange};
use std::sync::atomic::Ordering;
use std::sync::Arc;

pub struct AudioOutput {
    /// Mantener vivo el stream: al dropearlo se cierra el dispositivo.
    pub _stream: Stream,
    pub sample_rate: u32,
    pub channels: u16,
    pub buffer_frames: u32,
    pub device_name: String,
}

pub fn start(controls: Arc<Controls>, pending: PendingLoads, want_frames: u32) -> Result<AudioOutput> {
    let want_frames = std::env::var("DJDECK_FRAMES")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(want_frames);
    let host = cpal::default_host();
    let device = pick_device(&host)?;
    let device_name = match device.name() {
        Ok(n) if !n.is_empty() => n,
        _ => "desconocido".to_string(),
    };

    let (config, sample_format) = choose_config(&device)?;
    let sample_rate = config.sample_rate.0;
    let channels = config.channels;

    let build = |frames: BufferSize| -> Result<Stream> {
        let mut cfg = config.clone();
        cfg.buffer_size = frames;
        let mixer = Mixer::new(controls.clone(), pending.clone(), sample_rate);
        let err_controls = controls.clone();
        let err_fn = move |e: cpal::StreamError| {
            err_controls.xruns.fetch_add(1, Ordering::Relaxed);
            eprintln!("audio: {e}");
        };
        match sample_format {
            SampleFormat::F32 => {
                let mut mixer = mixer;
                device
                    .build_output_stream(
                        &cfg,
                        move |data: &mut [f32], _| mixer.render(data),
                        err_fn,
                        None,
                    )
                    .map_err(|e| anyhow!("{e}"))
            }
            SampleFormat::I16 => {
                let mut mixer = mixer;
                let mut scratch = vec![0.0f32; 16_384];
                device
                    .build_output_stream(
                        &cfg,
                        move |data: &mut [i16], _| {
                            if scratch.len() < data.len() {
                                scratch.resize(data.len(), 0.0);
                            }
                            let buf = &mut scratch[..data.len()];
                            mixer.render(buf);
                            for (dst, src) in data.iter_mut().zip(buf.iter()) {
                                *dst = (src.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                            }
                        },
                        err_fn,
                        None,
                    )
                    .map_err(|e| anyhow!("{e}"))
            }
            other => Err(anyhow!("formato de muestra no soportado: {other:?}")),
        }
    };

    let (stream, buffer_frames) = if want_frames == 0 {
        (build(BufferSize::Default)?, 0)
    } else {
        match build(BufferSize::Fixed(want_frames)) {
            Ok(s) => (s, want_frames),
            Err(e) => {
                eprintln!("audio: {want_frames} frames rechazados ({e}); usando el buffer del backend");
                (build(BufferSize::Default)?, 0)
            }
        }
    };

    stream.play()?;

    Ok(AudioOutput {
        _stream: stream,
        sample_rate,
        channels,
        buffer_frames,
        device_name,
    })
}

/// Elige el dispositivo de salida. `DJDECK_DEVICE` permite forzar uno por
/// subcadena del nombre (p. ej. `hw` para saltar PipeWire y hablar con ALSA).
fn pick_device(host: &cpal::Host) -> Result<cpal::Device> {
    if let Ok(want) = std::env::var("DJDECK_DEVICE") {
        if !want.is_empty() {
            if let Ok(devices) = host.output_devices() {
                for d in devices {
                    let name = d.name().unwrap_or_default();
                    if name.to_lowercase().contains(&want.to_lowercase()) {
                        eprintln!("audio: dispositivo forzado «{name}»");
                        return Ok(d);
                    }
                }
            }
            return Err(anyhow!("DJDECK_DEVICE={want}: no hay dispositivo con ese nombre"));
        }
    }
    host.default_output_device()
        .ok_or_else(|| anyhow!("no hay dispositivo de salida de audio"))
}

/// Prefiere 48 kHz estéreo; si no, el default del dispositivo.
fn choose_config(device: &cpal::Device) -> Result<(StreamConfig, SampleFormat)> {
    let wanted_rate: u32 = std::env::var("DJDECK_RATE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(48_000);

    if let Ok(ranges) = device.supported_output_configs() {
        let mut candidates: Vec<SupportedStreamConfigRange> = ranges.collect();
        // Primero f32 (el motor mezcla en f32), después i16.
        candidates.sort_by_key(|r| match r.sample_format() {
            SampleFormat::F32 => 0,
            SampleFormat::I16 => 1,
            _ => 2,
        });
        for range in candidates {
            if range.channels() != 2 {
                continue;
            }
            if !matches!(range.sample_format(), SampleFormat::F32 | SampleFormat::I16) {
                continue;
            }
            if range.min_sample_rate().0 <= wanted_rate && range.max_sample_rate().0 >= wanted_rate {
                let cfg = range.with_sample_rate(cpal::SampleRate(wanted_rate));
                return Ok((cfg.config(), range.sample_format()));
            }
        }
    }

    let default = device
        .default_output_config()
        .map_err(|e| anyhow!("no se pudo leer la config del dispositivo: {e}"))?;
    let format = default.sample_format();
    if !matches!(format, SampleFormat::F32 | SampleFormat::I16) {
        return Err(anyhow!("formato de muestra no soportado: {format:?}"));
    }
    Ok((default.config(), format))
}
