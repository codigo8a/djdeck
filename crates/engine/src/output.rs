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
use std::io::Write;
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// Tap de diagnóstico: `DJDECK_TAP=/ruta/salida.f32` graba exactamente los
/// frames que se le entregan al dispositivo (raw f32le entrelazado al SR del
/// device). Sirve para separar "la app genera basura" de "la basura aparece
/// después": se analiza el archivo y se sabe de qué lado está el problema.
struct Tap {
    w: std::io::BufWriter<std::fs::File>,
    escritas: usize,
}

const TAP_MAX_FRAMES: usize = 48_000 * 2 * 180; // 180 s

fn abrir_tap() -> Option<Tap> {
    let path = std::env::var("DJDECK_TAP").ok()?;
    let f = std::fs::File::create(&path).ok()?;
    println!("tap: grabando lo que va al dispositivo en {path} (f32le, 2 canales, 180 s máx)");
    Some(Tap {
        w: std::io::BufWriter::with_capacity(1 << 16, f),
        escritas: 0,
    })
}

pub struct AudioOutput {
    /// Mantener vivo el stream: al dropearlo se cierra el dispositivo.
    pub _stream: Stream,
    pub sample_rate: u32,
    pub channels: u16,
    pub buffer_frames: u32,
    pub device_name: String,
    pub sample_format: SampleFormat,
}

pub fn start(controls: Arc<Controls>, pending: PendingLoads, want_frames: u32) -> Result<AudioOutput> {
    let want_frames = std::env::var("DJDECK_FRAMES")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(want_frames);

    // pipewire-alsa negocia la latencia del nodo con esta variable. Sin ella el
    // cliente pide 256 frames y arrastra el grafo a ciclos de 5.33 ms, aunque la
    // app produzca bloques de 1024: el bloque no entra en un ciclo y PipeWire
    // descarta fragmentos (se escucha como chispeo). Se pide la misma latencia
    // que el buffer que vamos a usar.
    if std::env::var_os("PIPEWIRE_LATENCY").is_none() && want_frames > 0 {
        let sr_previo: u32 = std::env::var("DJDECK_RATE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(48_000);
        std::env::set_var("PIPEWIRE_LATENCY", format!("{want_frames}/{sr_previo}"));
    }
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
                let mut tap = abrir_tap();
                device
                    .build_output_stream(
                        &cfg,
                        move |data: &mut [f32], _| {
                            mixer.render(data);
                            if let Some(t) = tap.as_mut() {
                                if t.escritas < TAP_MAX_FRAMES {
                                    let bytes = unsafe {
                                        std::slice::from_raw_parts(
                                            data.as_ptr() as *const u8,
                                            data.len() * 4,
                                        )
                                    };
                                    let _ = t.w.write_all(bytes);
                                    t.escritas += data.len();
                                }
                            }
                        },
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
            SampleFormat::I32 => {
                let mut mixer = mixer;
                let mut scratch = vec![0.0f32; 16_384];
                device
                    .build_output_stream(
                        &cfg,
                        move |data: &mut [i32], _| {
                            if scratch.len() < data.len() {
                                scratch.resize(data.len(), 0.0);
                            }
                            let buf = &mut scratch[..data.len()];
                            mixer.render(buf);
                            for (dst, src) in data.iter_mut().zip(buf.iter()) {
                                *dst = (src.clamp(-1.0, 1.0) * i32::MAX as f32) as i32;
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
        sample_format,
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
///
/// `DJDECK_FORMAT=s16|s32|f32` fuerza el formato de muestra. Importa: medido en
/// este equipo, el camino de **float** del plugin de ALSA mete chispeo leve
/// (dos regrabaciones: F32 con chispa, S16 limpio), y la app hablaba en F32.
fn choose_config(device: &cpal::Device) -> Result<(StreamConfig, SampleFormat)> {
    let wanted_rate: u32 = std::env::var("DJDECK_RATE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(48_000);

    let preferido = match std::env::var("DJDECK_FORMAT").ok().as_deref() {
        Some("s16") => SampleFormat::I16,
        Some("s32") => SampleFormat::I32,
        _ => SampleFormat::F32,
    };

    if let Ok(ranges) = device.supported_output_configs() {
        let mut candidates: Vec<SupportedStreamConfigRange> = ranges.collect();
        // Primero el pedido, después el resto en orden de preferencia.
        candidates.sort_by_key(|r| {
            let f = r.sample_format();
            if f == preferido {
                0
            } else {
                match f {
                    SampleFormat::I16 => 1,
                    SampleFormat::I32 => 2,
                    SampleFormat::F32 => 3,
                    _ => 4,
                }
            }
        });
        for range in candidates {
            if range.channels() != 2 {
                continue;
            }
            if !matches!(
                range.sample_format(),
                SampleFormat::F32 | SampleFormat::I16 | SampleFormat::I32
            ) {
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
