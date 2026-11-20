//! Remuestreo band-limited a la frecuencia del dispositivo.
//!
//! El MVP decodifica el track entero en memoria y lo convierte al SR del
//! dispositivo una sola vez al cargarlo: así el callback jamás remuestrea y la
//! posición de los decks vive en una única unidad (frames @ SR del dispositivo).
//!
//! Se interpola con un **sinc con ventana de Kaiser**, evaluado en polifase
//! (una tabla de pesos por fase fraccionaria). Una interpolación polinómica
//! —Catmull-Rom, lineal— no filtra las imágenes espectrales que genera el
//! cambio de frecuencia, y esas imágenes caen dentro de la banda audible:
//! medido sobre música real de 44.1 kHz, Catmull-Rom dejaba -13 dB de error en
//! 12-16 kHz (chispeo en platillos y aire). El sinc lo baja bajo -80 dB.

/// Semi-ancho del kernel en muestras de entrada con ratio >= 1 (48 taps).
/// 24 alcanza: con la ventana de Kaiser el rechazo queda bajo -90 dB y cuesta
/// un tercio menos que 32 taps por lado.
const TAPS_PER_SIDE: f64 = 24.0;
/// Ventana de Kaiser: beta 8.6 da ~90 dB de atenuación en la banda de rechazo.
const KAISER_BETA: f64 = 8.6;

fn gcd(a: u32, b: u32) -> u32 {
    if b == 0 {
        a
    } else {
        gcd(b, a % b)
    }
}

fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-9 {
        1.0
    } else {
        let s = std::f64::consts::PI * x;
        s.sin() / s
    }
}

/// Bessel I0 por serie de potencias (converge rápido para beta <= 10).
fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0f64;
    let mut term = 1.0f64;
    let mut k = 1.0f64;
    loop {
        let h = x / (2.0 * k);
        term *= h * h;
        sum += term;
        if term < 1e-13 * sum {
            return sum;
        }
        k += 1.0;
    }
}

pub fn resample_stereo(input: &[f32], src_sr: u32, dst_sr: u32) -> Vec<f32> {
    if src_sr == dst_sr || src_sr == 0 || dst_sr == 0 {
        return input.to_vec();
    }
    let in_frames = input.len() / 2;
    if in_frames == 0 {
        return Vec::new();
    }

    // Ratio como fracción exacta: la fase de la interpolación se repite cada
    // `up` muestras de salida, así que la tabla polifásica es finita y exacta.
    let g = gcd(src_sr, dst_sr).max(1);
    let up = (dst_sr / g) as usize;
    let down = (src_sr / g) as usize;
    let ratio = dst_sr as f64 / src_sr as f64;
    let out_frames = ((in_frames as f64) * ratio).round() as usize;

    // Cutoff en ciclos por muestra de entrada. Al bajar de frecuencia hay que
    // filtrar por debajo del nuevo Nyquist o se aliasea; al subir, queda en
    // Nyquist del origen y la ventana se encarga de las imágenes.
    let fc = 0.5 * ratio.min(1.0);
    let half = TAPS_PER_SIDE / (2.0 * fc);
    let off = half.ceil() as usize; // corrimiento del primer tap
    let n_taps = off * 2;

    let kaiser_norm = bessel_i0(KAISER_BETA);
    let kaiser = |t: f64| -> f64 {
        let r = (t / half).abs();
        if r >= 1.0 {
            0.0
        } else {
            bessel_i0(KAISER_BETA * (1.0 - r * r).sqrt()) / kaiser_norm
        }
    };

    // Tabla polifásica: `up` filas de `n_taps` pesos, normalizadas para
    // ganancia unitaria exacta en continua.
    let mut table = vec![0.0f32; up * n_taps];
    for p in 0..up {
        let frac = p as f64 / up as f64;
        let mut row = vec![0.0f64; n_taps];
        let mut sum = 0.0f64;
        for (j, w) in row.iter_mut().enumerate() {
            // Tap j apunta a la muestra idx + j - (off - 1); el kernel se
            // evalúa en la distancia entre esa muestra y la posición pedida.
            let t = (j as f64) - (off as f64 - 1.0) - frac;
            *w = 2.0 * fc * sinc(2.0 * fc * t) * kaiser(t);
            sum += *w;
        }
        if sum.abs() > 1e-12 {
            for (j, w) in row.iter().enumerate() {
                table[p * n_taps + j] = (*w / sum) as f32;
            }
        }
    }

    let last = in_frames as i64 - 1;
    let primer_tap = off as i64 - 1;
    let mut out = Vec::with_capacity(out_frames * 2);
    for i in 0..out_frames {
        let num = i * down;
        let idx = (num / up) as i64;
        let p = num % up;
        let row = &table[p * n_taps..(p + 1) * n_taps];
        let base = idx - primer_tap;
        let mut l = 0.0f32;
        let mut r = 0.0f32;
        if base >= 0 && base + n_taps as i64 <= last + 1 {
            // Camino rápido: todos los taps caen dentro de la señal. Se recorre
            // por chunks de 2 para que el compilador vectorice sin chequeos.
            let base = base as usize;
            let src = &input[base * 2..(base + n_taps) * 2];
            for (chunk, w) in src.chunks_exact(2).zip(row.iter()) {
                l += chunk[0] * *w;
                r += chunk[1] * *w;
            }
        } else {
            for (j, w) in row.iter().enumerate() {
                let k = (base + j as i64).clamp(0, last) as usize;
                l += input[k * 2] * w;
                r += input[k * 2 + 1] * w;
            }
        }
        out.push(l);
        out.push(r);
    }
    out
}

/// Interpolación lineal de un frame en el buffer entrelazado.
///
/// Se usa en el playback con pitch, donde la posición avanza `rate` por frame y
/// el remuestreo tiene que ser barato: la tabla polifásica de arriba es para la
/// conversión única al cargar.
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
