//! Remuestreo Catmull-Rom a la frecuencia del dispositivo.
//!
//! El MVP decodifica el track entero en memoria y lo convierte al SR del
//! dispositivo una sola vez al cargarlo: así el callback jamás remuestrea y la
//! posición de los decks vive en una única unidad (frames @ SR del dispositivo).

pub fn resample_stereo(input: &[f32], src_sr: u32, dst_sr: u32) -> Vec<f32> {
    if src_sr == dst_sr || src_sr == 0 || dst_sr == 0 {
        return input.to_vec();
    }
    let in_frames = input.len() / 2;
    if in_frames == 0 {
        return Vec::new();
    }

    let ratio = dst_sr as f64 / src_sr as f64;
    let out_frames = ((in_frames as f64) * ratio).round() as usize;
    let mut out = Vec::with_capacity(out_frames * 2);

    let last = in_frames as isize - 1;
    let sample = |idx: isize, ch: usize| -> f32 {
        let i = idx.clamp(0, last) as usize;
        input[i * 2 + ch]
    };

    for i in 0..out_frames {
        let pos = i as f64 / ratio;
        let idx = pos.floor() as isize;
        let t = (pos - idx as f64) as f32;

        for ch in 0..2 {
            let p0 = sample(idx - 1, ch);
            let p1 = sample(idx, ch);
            let p2 = sample(idx + 1, ch);
            let p3 = sample(idx + 2, ch);
            // Catmull-Rom
            let a = -0.5 * p0 + 1.5 * p1 - 1.5 * p2 + 0.5 * p3;
            let b = p0 - 2.5 * p1 + 2.0 * p2 - 0.5 * p3;
            let c = -0.5 * p0 + 0.5 * p2;
            let d = p1;
            out.push(((a * t + b) * t + c) * t + d);
        }
    }

    out
}

/// Interpolación lineal de un frame en el buffer entrelazado.
#[inline]
pub fn frame_at(samples: &[f32], pos: f64) -> (f32, f32) {
    let frames = samples.len() / 2;
    if frames == 0 {
        return (0.0, 0.0);
    }
    let pos = pos.clamp(0.0, (frames - 1) as f64);
    let idx = pos.floor() as usize;
    let t = (pos - idx as f64) as f32;
    let next = (idx + 1).min(frames - 1);
    let l0 = samples[idx * 2];
    let r0 = samples[idx * 2 + 1];
    let l1 = samples[next * 2];
    let r1 = samples[next * 2 + 1];
    (l0 + (l1 - l0) * t, r0 + (r1 - r0) * t)
}
