//! Análisis offline: BPM + primer beat por envolvente de energía, y picos para
//! la waveform.
//!
//! Esto es deliberadamente simple: autocorrelación de la envolvente de onsets.
//! Funciona bien en material de pulso claro (four-on-the-floor) y es
//! determinístico y testeable con click-tracks sintéticos. El detector neuronal
//! (`beat-this`) entra después, con golden set, no antes.

/// Resolución de la envolvente: una muestra cada 5 ms.
const HOP_HZ: f64 = 200.0;

pub struct Peaks {
    pub min: Vec<f32>,
    pub max: Vec<f32>,
}

/// Detecta BPM y posición del primer beat (segundos). `samples` es estéreo
/// entrelazado a `sample_rate`.
pub fn detect_bpm(samples: &[f32], sample_rate: u32) -> Option<(f64, f64)> {
    let onset = onset_envelope(samples, sample_rate);
    if onset.len() < 64 {
        return None;
    }
    let bpm = best_bpm(&onset)?;
    let first_beat = first_beat_position(&onset, bpm);
    Some((bpm, first_beat))
}

/// Envolvente de onsets: energía por hop, diferenciada en positivo.
fn onset_envelope(samples: &[f32], sample_rate: u32) -> Vec<f32> {
    let hop = ((sample_rate as f64 / HOP_HZ).round() as usize).max(1);
    let frames = samples.len() / 2;
    let n = frames / hop;
    if n < 4 {
        return Vec::new();
    }

    let mut energy = vec![0.0f32; n];
    for (i, slot) in energy.iter_mut().enumerate() {
        let start = i * hop;
        let end = (start + hop).min(frames);
        let mut acc = 0.0f32;
        for f in start..end {
            acc += samples[f * 2].abs() + samples[f * 2 + 1].abs();
        }
        *slot = acc / (2 * (end - start).max(1)) as f32;
    }

    let mut onset = vec![0.0f32; n];
    for i in 1..n {
        let d = energy[i] - energy[i - 1];
        onset[i] = d.max(0.0);
    }
    // Suavizado de 3 puntos para estabilizar la autocorrelación.
    let mut smooth = vec![0.0f32; n];
    for i in 0..n {
        let a = onset[i.saturating_sub(1)];
        let b = onset[i];
        let c = onset[(i + 1).min(n - 1)];
        smooth[i] = (a + 2.0 * b + c) * 0.25;
    }
    smooth
}

fn best_bpm(onset: &[f32]) -> Option<f64> {
    let n = onset.len();
    let mean = onset.iter().sum::<f32>() / n as f32;
    let x: Vec<f32> = onset.iter().map(|v| v - mean).collect();

    let lag_min = (HOP_HZ * 60.0 / 200.0).round() as usize; // 200 BPM
    let lag_max = (HOP_HZ * 60.0 / 55.0).round() as usize; // 55 BPM
    let lag_max = lag_max.min(n / 2);
    if lag_max <= lag_min + 2 {
        return None;
    }

    let mut scores = vec![0.0f64; lag_max + 1];
    for lag in lag_min..=lag_max {
        let m = n - lag;
        if m == 0 {
            continue;
        }
        let mut acc = 0.0f64;
        for i in 0..m {
            acc += (x[i] * x[i + lag]) as f64;
        }
        scores[lag] = acc / m as f64;
    }

    let mut best_lag = lag_min;
    let mut best = f64::NEG_INFINITY;
    for lag in lag_min..=lag_max {
        if scores[lag] > best {
            best = scores[lag];
            best_lag = lag;
        }
    }
    if best <= 0.0 {
        return None;
    }

    // Refinado parabólico del pico de correlación.
    let mut lag = best_lag as f64;
    if best_lag > lag_min && best_lag < lag_max {
        let (a, b, c) = (scores[best_lag - 1], scores[best_lag], scores[best_lag + 1]);
        let denom = a - 2.0 * b + c;
        if denom.abs() > 1e-12 {
            lag += 0.5 * (a - c) / denom;
        }
    }

    let mut bpm = 60.0 * HOP_HZ / lag;
    // Prior de tempo para música de DJ: fuera de [70, 190] se corrige por octava.
    while bpm < 70.0 {
        bpm *= 2.0;
    }
    while bpm > 190.0 {
        bpm /= 2.0;
    }
    Some(bpm)
}

/// Residuo de la rejilla que maximiza la energía de onset: posición del primer
/// beat. Precisión de un hop (5 ms) más el refinado al pico local.
fn first_beat_position(onset: &[f32], bpm: f64) -> f64 {
    let n = onset.len();
    let period = HOP_HZ * 60.0 / bpm;
    if period < 2.0 || n == 0 {
        return 0.0;
    }

    let steps = period.floor().max(1.0) as usize;
    let mut best_phase = 0.0f64;
    let mut best_score = f64::NEG_INFINITY;
    for r in 0..steps {
        let mut acc = 0.0f64;
        let mut t = r as f64;
        while (t as usize) < n {
            acc += onset[t as usize] as f64;
            t += period;
        }
        if acc > best_score {
            best_score = acc;
            best_phase = r as f64;
        }
    }

    // Refinado: el pico de onset real cerca de la fase elegida.
    let mut best_idx = best_phase as usize;
    let mut best_val = f32::NEG_INFINITY;
    let lo = (best_phase as isize - 2).max(0) as usize;
    let hi = ((best_phase as usize) + 3).min(n);
    for i in lo..hi {
        if onset[i] > best_val {
            best_val = onset[i];
            best_idx = i;
        }
    }
    best_idx as f64 / HOP_HZ
}

/// Pirámide min/max para dibujar la waveform: `buckets` columnas.
pub fn peaks(samples: &[f32], buckets: usize) -> Peaks {
    let frames = samples.len() / 2;
    let buckets = buckets.max(1);
    if frames == 0 {
        return Peaks {
            min: vec![0.0; buckets],
            max: vec![0.0; buckets],
        };
    }
    let mut min = vec![0.0f32; buckets];
    let mut max = vec![0.0f32; buckets];
    for b in 0..buckets {
        let start = b * frames / buckets;
        let end = ((b + 1) * frames / buckets).max(start + 1).min(frames);
        let mut lo = f32::INFINITY;
        let mut hi = f32::NEG_INFINITY;
        for f in start..end {
            let v = (samples[f * 2] + samples[f * 2 + 1]) * 0.5;
            lo = lo.min(v);
            hi = hi.max(v);
        }
        min[b] = lo;
        max[b] = hi;
    }
    Peaks { min, max }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn click_track(bpm: f64, seconds: f64, sr: u32, first_beat: f64) -> Vec<f32> {
        let frames = (seconds * sr as f64) as usize;
        let mut out = vec![0.0f32; frames * 2];
        let period = 60.0 / bpm;
        let mut t = first_beat;
        while t < seconds {
            let start = (t * sr as f64) as usize;
            // Click de 10 ms con caída exponencial.
            for i in 0..(sr as usize / 100) {
                let idx = start + i;
                if idx >= frames {
                    break;
                }
                let env = (-(i as f32) / (sr as f32 * 0.002)).exp();
                let v = (i as f32 * 2.0 * std::f32::consts::PI * 1000.0 / sr as f32).sin() * env * 0.6;
                out[idx * 2] += v;
                out[idx * 2 + 1] += v;
            }
            t += period;
        }
        out
    }

    #[test]
    fn detecta_128_bpm() {
        let sr = 44_100;
        let s = click_track(128.0, 20.0, sr, 0.5);
        let (bpm, first) = detect_bpm(&s, sr).expect("debe detectar");
        assert!((bpm - 128.0).abs() < 0.5, "bpm={bpm}");
        // `first_beat` identifica la FASE de la rejilla, no el primer downbeat
        // musical: se compara módulo un beat.
        let beat = 60.0 / bpm;
        let d = ((first - 0.5) % beat + beat) % beat;
        let d = d.min(beat - d);
        assert!(d < 0.03, "fase de rejilla corrida {d}s (first={first})");
    }

    #[test]
    fn detecta_90_bpm() {
        let sr = 48_000;
        let s = click_track(90.0, 20.0, sr, 0.25);
        let (bpm, first) = detect_bpm(&s, sr).expect("debe detectar");
        assert!((bpm - 90.0).abs() < 0.5, "bpm={bpm}");
        assert!((first - 0.25).abs() < 0.03, "first_beat={first}");
    }

    #[test]
    fn peaks_acotados() {
        let s = vec![0.5f32; 2000];
        let p = peaks(&s, 100);
        assert_eq!(p.max.len(), 100);
        assert!(p.max.iter().all(|v| (*v - 0.5).abs() < 1e-6));
    }
}
