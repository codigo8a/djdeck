//! Decodificación a f32 estéreo entrelazado usando symphonia.

use anyhow::{anyhow, Context, Result};
use std::fs::File;
use std::path::Path;
use symphonia::core::audio::{AudioBufferRef, SampleBuffer};
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

pub struct DecodedAudio {
    /// Muestras entrelazadas L,R,L,R...
    pub samples: Vec<f32>,
    pub sample_rate: u32,
}

impl DecodedAudio {
    pub fn frames(&self) -> usize {
        self.samples.len() / 2
    }

    pub fn duration_s(&self) -> f64 {
        self.frames() as f64 / self.sample_rate as f64
    }
}

pub fn decode_file(path: &Path) -> Result<DecodedAudio> {
    let file = File::open(path).with_context(|| format!("no se pudo abrir {}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions {
                enable_gapless: true,
                ..Default::default()
            },
            &MetadataOptions::default(),
        )
        .with_context(|| format!("formato no reconocido: {}", path.display()))?;

    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| anyhow!("el archivo no tiene pista de audio"))?
        .clone();

    let track_id = track.id;
    let mut sample_rate = track.codec_params.sample_rate.unwrap_or(44_100);
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .with_context(|| "códec no soportado")?;

    let mut samples: Vec<f32> = Vec::new();
    // Estimación de capacidad para no reallocar en un archivo grande.
    if let (Some(n_frames), Some(_)) = (track.codec_params.n_frames, track.codec_params.sample_rate) {
        samples.reserve(n_frames as usize * 2);
    }

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(SymError::ResetRequired) => break,
            Err(e) => return Err(e.into()),
        };
        if packet.track_id() != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(decoded) => {
                sample_rate = decoded.spec().rate;
                push_as_stereo(decoded, &mut samples);
            }
            // Paquetes corruptos: se saltan, como hace cualquier reproductor.
            Err(SymError::DecodeError(_)) => continue,
            Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.into()),
        }
    }

    if samples.is_empty() {
        return Err(anyhow!("se decodificaron 0 muestras de {}", path.display()));
    }

    Ok(DecodedAudio {
        samples,
        sample_rate,
    })
}

fn push_as_stereo(decoded: AudioBufferRef<'_>, out: &mut Vec<f32>) {
    let spec = *decoded.spec();
    let frames = decoded.frames();
    let channels = spec.channels.count();
    if frames == 0 {
        return;
    }

    let mut buf = SampleBuffer::<f32>::new(frames as u64, spec);
    buf.copy_interleaved_ref(decoded);
    let s = buf.samples();

    match channels {
        1 => {
            for i in 0..frames {
                let v = s[i];
                out.push(v);
                out.push(v);
            }
        }
        2 => out.extend_from_slice(&s[..frames * 2]),
        n => {
            let half = (n / 2).max(1) as f32;
            for i in 0..frames {
                let mut l = 0.0;
                let mut r = 0.0;
                for c in 0..n {
                    let v = s[i * n + c];
                    if c < n / 2 {
                        l += v;
                    } else {
                        r += v;
                    }
                }
                out.push(l / half);
                out.push(r / half);
            }
        }
    }
}
