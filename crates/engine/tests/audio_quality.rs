//! Calidad de audio medida, no supuesta: rechazo de imágenes del remuestreo,
//! distorsión del camino de pitch, bypass del EQ y comportamiento del limiter.
//!
//! Cada umbral está puesto en el nivel medido hoy, con margen, y documentado en
//! el test. La forma es deliberada: se afirma una cota superior de distorsión,
//! así un arreglo que mejore la calidad sigue pasando y una regresión falla.
//! Los casos marcados "deuda" son defectos conocidos que el test fija para que
//! no empeoren; cuando se arreglen, el umbral se aprieta.

use std::f64::consts::PI;
use std::sync::{Arc, Mutex};

use engine::sync::BeatGrid;
use engine::{set_f32, Controls, Eq, Mixer, PendingLoads, Track};

const SR: u32 = 48_000;
/// 16k puntos: 2.93 Hz por bin, suficiente para ver espurios in-band.
const N_FFT: usize = 16_384;
/// Tono generado y renderizado.
const RENDER_FRAMES: usize = SR as usize;
/// La fuente dura más que el render: con rate > 1 el deck no debe quedarse sin
/// material, o el silencio del final se mide como distorsión.
const SOURCE_FRAMES: usize = SR as usize * 3;
/// 100 ms de transitorio descartados antes de medir.
const SKIP: usize = 4_800;

// ---------------------------------------------------------------------------
// Generación y render por el camino real del motor
// ---------------------------------------------------------------------------

fn sine(freq: f64, frames: usize, amp: f32, sr: f64) -> Vec<f32> {
    let mut v = Vec::with_capacity(frames * 2);
    for i in 0..frames {
        let s = (2.0 * PI * freq * i as f64 / sr).sin() as f32 * amp;
        v.push(s);
        v.push(s);
    }
    v
}

struct Settings {
    rate: f64,
    gain: f32,
    /// 0.0 = deck A a fondo; 0.5 = centro equal-power.
    crossfader: f32,
    master: f32,
    eq: (f32, f32, f32),
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            rate: 1.0,
            gain: 1.0,
            crossfader: 0.0,
            master: 1.0,
            eq: (0.0, 0.0, 0.0),
        }
    }
}

/// Carga los tracks, deja el motor en `settings` y renderiza 1 s.
fn render(tracks: Vec<(usize, Vec<f32>)>, s: &Settings) -> Vec<f32> {
    let controls = Arc::new(Controls::default());
    set_f32(&controls.master_gain, s.master);
    set_f32(&controls.crossfader, s.crossfader);
    let pending: PendingLoads = Arc::new(Mutex::new(Vec::new()));
    {
        let mut q = pending.lock().unwrap();
        for (deck, samples) in tracks {
            q.push((
                deck,
                Arc::new(Track::new("test", samples, SR, BeatGrid::new(120.0, 0.0))),
            ));
        }
    }
    let mut mixer = Mixer::new(controls.clone(), pending.clone(), SR);
    // Primer bloque: consume la cola de cargas. Sin play todavía, sale silencio.
    let mut warm = vec![0.0f32; 256 * 2];
    mixer.render(&mut warm);

    for d in 0..2 {
        set_f32(&controls.rate[d], s.rate as f32);
        set_f32(&controls.gain[d], s.gain);
        set_f32(&controls.eq_low[d], s.eq.0);
        set_f32(&controls.eq_mid[d], s.eq.1);
        set_f32(&controls.eq_high[d], s.eq.2);
        controls.playing[d].store(true, std::sync::atomic::Ordering::Relaxed);
    }
    mixer.render_to_vec(RENDER_FRAMES)
}

fn peak(y: &[f32]) -> f32 {
    y.iter().fold(0.0f32, |m, v| m.max(v.abs()))
}

// ---------------------------------------------------------------------------
// Análisis: FFT con ventana Blackman-Harris (lóbulos laterales -92 dB)
// ---------------------------------------------------------------------------

fn fft(re: &mut [f64], im: &mut [f64]) {
    let n = re.len();
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = -2.0 * PI / len as f64;
        for i in (0..n).step_by(len) {
            for k in 0..len / 2 {
                let (wr, wi) = ((ang * k as f64).cos(), (ang * k as f64).sin());
                let (ur, ui) = (re[i + k], im[i + k]);
                let vr = re[i + k + len / 2] * wr - im[i + k + len / 2] * wi;
                let vi = re[i + k + len / 2] * wi + im[i + k + len / 2] * wr;
                re[i + k] = ur + vr;
                im[i + k] = ui + vi;
                re[i + k + len / 2] = ur - vr;
                im[i + k + len / 2] = ui - vi;
            }
        }
        len <<= 1;
    }
}

struct Spectrum {
    /// Energía in-band (hasta 20 kHz) que no es la fundamental, en dB relativos.
    inband_db: f64,
    /// Energía por encima de 20 kHz, en dB relativos.
    ultrasonic_db: f64,
    /// Los espurios más grandes, como (frecuencia Hz, dB relativos).
    spurs: Vec<(f64, f64)>,
}

fn spectrum(y: &[f32], fundamental: f64) -> Spectrum {
    let mut re = vec![0.0f64; N_FFT];
    let mut im = vec![0.0f64; N_FFT];
    for i in 0..N_FFT {
        let x = y[(SKIP + i) * 2] as f64;
        let t = 2.0 * PI * i as f64 / N_FFT as f64;
        let w = 0.35875 - 0.48829 * t.cos() + 0.14128 * (2.0 * t).cos()
            - 0.01168 * (3.0 * t).cos();
        re[i] = x * w;
    }
    fft(&mut re, &mut im);
    let bin_hz = SR as f64 / N_FFT as f64;
    let mag: Vec<f64> = (0..N_FFT / 2)
        .map(|k| (re[k] * re[k] + im[k] * im[k]).sqrt())
        .collect();
    let fund_bin = (fundamental / bin_hz).round() as usize;
    // La fundamental puede caer entre bins: se suma todo su lóbulo principal.
    let fund_pow: f64 = (fund_bin.saturating_sub(4)..(fund_bin + 5))
        .map(|k| mag[k] * mag[k])
        .sum();

    let mut inband = 0.0;
    let mut ultrasonic = 0.0;
    let mut spurs: Vec<(f64, f64)> = Vec::new();
    for k in 1..N_FFT / 2 {
        let f = k as f64 * bin_hz;
        if (f - fundamental).abs() < 20.0 * bin_hz {
            continue;
        }
        let p = mag[k] * mag[k];
        if f < 20_000.0 {
            inband += p;
        } else {
            ultrasonic += p;
        }
        spurs.push((f, 10.0 * (p / fund_pow).log10()));
    }
    spurs.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    spurs.truncate(5);
    Spectrum {
        inband_db: 10.0 * (inband / fund_pow).log10(),
        ultrasonic_db: 10.0 * (ultrasonic / fund_pow).log10(),
        spurs,
    }
}

/// Distorsión in-band del render de un tono.
fn inband_db(y: &[f32], fundamental: f64) -> f64 {
    spectrum(y, fundamental).inband_db
}

fn describe(y: &[f32], fundamental: f64) -> String {
    let s = spectrum(y, fundamental);
    let spurs: Vec<String> = s
        .spurs
        .iter()
        .map(|(f, d)| format!("{f:.0} Hz {d:.1} dB"))
        .collect();
    format!(
        "in-band {:.1} dB, >20 kHz {:.1} dB, pico {:.3}, espurios: {}",
        s.inband_db,
        s.ultrasonic_db,
        peak(y),
        spurs.join(", ")
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn pitch_cero_es_bit_exacto() {
    // Con rate exacto 1.0 el deck lee índices enteros: la salida debe ser la
    // entrada sin tocar. Es la referencia contra la que se mide todo lo demás.
    let src = sine(1000.0, SOURCE_FRAMES, 0.5, SR as f64);
    let y = render(vec![(0, src.clone())], &Settings::default());
    let max_diff = (0..RENDER_FRAMES)
        .map(|i| (y[i * 2] - src[i * 2]).abs())
        .fold(0.0f32, f32::max);
    assert!(max_diff <= 1e-7, "rate 1.0 debe ser transparente, desvío {max_diff:e}");
}

#[test]
fn eq_en_cero_db_es_bypass_exacto() {
    // El bypass no es "casi": con las tres bandas en 0 dB las muestras salen
    // idénticas a la entrada.
    let mut eq = Eq::new(SR);
    eq.set_gains(0.0, 0.0, 0.0, SR);
    let mut max_diff = 0.0f32;
    for i in 0..1024 {
        let x = (i as f32 * 0.01).sin() * 0.7;
        let (l, r) = eq.process_stereo(x, -x);
        max_diff = max_diff.max((l - x).abs()).max((r + x).abs());
    }
    assert_eq!(max_diff, 0.0, "el bypass debe ser bit-exacto, desvío {max_diff:e}");
}

#[test]
fn eq_activo_no_agrega_distorsion_propia() {
    // El EQ es lineal: se prueba por escalado y superposición, no por THD
    // absoluto. Un shelf de graves tiene un piso de transitorio de asentamiento
    // que contamina la medición absoluta; el escalado lo cancela porque los dos
    // renders comparten la misma forma de entrada.
    // Niveles bajos a propósito: el limiter queda fuera (pico < 0.8), así lo
    // único no lineal del camino sería el propio EQ.
    let eq = (12.0, -12.0, 12.0);
    let base = Settings { gain: 0.8, master: 0.9, eq, ..Default::default() };
    let x = sine(1000.0, SOURCE_FRAMES, 0.15, SR as f64);
    let z = sine(3000.0, SOURCE_FRAMES, 0.15, SR as f64);
    let doble: Vec<f32> = x.iter().map(|v| v * 2.0).collect();
    let suma: Vec<f32> = x.iter().zip(z.iter()).map(|(a, b)| a + b).collect();

    let y_x = render(vec![(0, x)], &base);
    let y_2x = render(vec![(0, doble)], &base);
    let y_z = render(vec![(0, z)], &base);
    let y_suma = render(vec![(0, suma)], &base);

    let rel = |a: &[f32], b: &[f32]| -> f32 {
        let (mut num, mut den) = (0.0f64, 0.0f64);
        for i in SKIP..RENDER_FRAMES {
            let d = a[i * 2] as f64 - b[i * 2] as f64;
            num += d * d;
            den += (b[i * 2] as f64) * (b[i * 2] as f64);
        }
        (num / den).sqrt() as f32
    };

    let error_escalado = rel(
        &y_2x,
        &y_x.iter().map(|v| v * 2.0).collect::<Vec<f32>>(),
    );
    let error_superposicion = rel(
        &y_suma,
        &y_x.iter().zip(y_z.iter()).map(|(a, b)| a + b).collect::<Vec<f32>>(),
    );
    assert!(
        error_escalado < 1e-4,
        "el EQ debe escalar lineal (y(2x) = 2y(x)), error relativo {error_escalado:e}"
    );
    assert!(
        error_superposicion < 1e-4,
        "el EQ debe cumplir superposición, error relativo {error_superposicion:e}"
    );
}

#[test]
fn limiter_es_transparente_bajo_el_umbral() {
    // Hasta 0.8 de entrada el soft_clip devuelve la muestra sin tocar
    // (medido: -102.8 dB, el piso de la ventana).
    let p = Settings { gain: 0.8, ..Default::default() };
    let y = render(vec![(0, sine(1000.0, SOURCE_FRAMES, 1.0, SR as f64))], &p);
    let d = inband_db(&y, 1000.0);
    assert!(d <= -95.0, "bajo el umbral no debe distorsionar: {}", describe(&y, 1000.0));
}

#[test]
fn limiter_por_encima_del_umbral_distorsiona() {
    // Deuda conocida: el soft_clip es un tanh, no un limitador con lookahead.
    // A 1.35 de entrada mide -18.9 dB con un armónico en 3 kHz. El techo sí se
    // respeta: nunca pasa de 1.0.
    let p = Settings { gain: 1.5, ..Default::default() };
    let y = render(vec![(0, sine(1000.0, SOURCE_FRAMES, 0.9, SR as f64))], &p);
    let d = inband_db(&y, 1000.0);
    assert!(d <= -12.0, "no debe empeorar de -18.9 dB: {}", describe(&y, 1000.0));
    assert!(peak(&y) <= 1.0 + 1e-6, "el techo del limiter se respeta, pico {}", peak(&y));
}

#[test]
fn resample_44_1_a_48_mantiene_los_graves_limpios() {
    let src = sine(1000.0, SOURCE_FRAMES, 0.4, 44_100.0);
    let out = engine::resample_stereo(&src, 44_100, SR);
    let d = inband_db(&out, 1000.0);
    assert!(d <= -80.0, "1 kHz debe quedar limpio (medido -89.3 dB): {}", describe(&out, 1000.0));
}

#[test]
fn resample_44_1_a_48_rechaza_imagenes_en_agudos() {
    // Deuda conocida y la causa más audible con archivos reales: Catmull-Rom no
    // tiene filtro anti-imagen. Un tono de 10 kHz deja una imagen en 44.1-10 =
    // 34.1 kHz que en un stream de 48 kHz se pliega a 13.9 kHz, a -30 dB; y uno
    // de 15 kHz plegado a 18.9 kHz a -17 dB. Se fija el nivel actual para que no
    // empeore: un sinc/polyphase lo baja a -80 dB y estos tests siguen pasando.
    let out10 = engine::resample_stereo(&sine(10_000.0, SOURCE_FRAMES, 0.4, 44_100.0), 44_100, SR);
    let d10 = inband_db(&out10, 10_000.0);
    assert!(d10 <= -25.0, "imagen de 10 kHz no debe empeorar de -26.4 dB: {}", describe(&out10, 10_000.0));

    let out15 = engine::resample_stereo(&sine(15_000.0, SOURCE_FRAMES, 0.4, 44_100.0), 44_100, SR);
    let d15 = inband_db(&out15, 15_000.0);
    assert!(d15 <= -12.0, "imagen de 15 kHz no debe empeorar de -13.8 dB: {}", describe(&out15, 15_000.0));
}

#[test]
fn pitch_limpio_en_graves_y_medios() {
    // frame_at interpola lineal, pero a 1 kHz el error es despreciable
    // (medido: -63.8 dB a rate 1.08, -64 dB a 0.92 y 1.02).
    for rate in [1.02, 1.08, 0.92] {
        let p = Settings { rate, ..Default::default() };
        let y = render(vec![(0, sine(1000.0, SOURCE_FRAMES, 0.5, SR as f64))], &p);
        let d = inband_db(&y, 1000.0 * rate);
        assert!(d <= -55.0, "rate {rate}: {}", describe(&y, 1000.0 * rate));
    }
}

#[test]
fn pitch_ensucia_los_agudos() {
    // Deuda conocida: el patrón de interpolación lineal modula la señal y crea
    // bandas laterales in-band que crecen con la frecuencia. A rate 1.08 el
    // error in-band es -35.5 dB en 5 kHz, -22.2 dB en 10 kHz y -13.5 dB en
    // 15 kHz. Un interpolador cúbico en frame_at debería bajarlo ~40 dB.
    for (freq, limite) in [(5_000.0, -30.0), (10_000.0, -18.0), (15_000.0, -10.0)] {
        let p = Settings { rate: 1.08, ..Default::default() };
        let y = render(vec![(0, sine(freq, SOURCE_FRAMES, 0.4, SR as f64))], &p);
        let d = inband_db(&y, freq * 1.08);
        assert!(d <= limite, "{freq} Hz a rate 1.08 no debe empeorar de {limite} dB: {}", describe(&y, freq * 1.08));
    }
}

#[test]
fn eq_mid_alto_satura_el_limiter() {
    // Deuda conocida: el EQ va antes del limiter y no compensa headroom, así que
    // un boost de +12 dB en el mid (+4x) mete 1 kHz de 0.5 en el tanh: pico 1.0
    // y -12.9 dB de distorsión con armónico en 3 kHz. Con headroom de -6 dB
    // después del EQ esto debería quedar bajo -60 dB.
    let p = Settings { eq: (0.0, 12.0, 0.0), ..Default::default() };
    let y = render(vec![(0, sine(1000.0, SOURCE_FRAMES, 0.5, SR as f64))], &p);
    let d = inband_db(&y, 1000.0);
    assert!(d <= -10.0, "no debe empeorar de -12.9 dB: {}", describe(&y, 1000.0));
    assert!(peak(&y) <= 1.0 + 1e-6, "el techo se respeta, pico {}", peak(&y));
}

#[test]
fn dos_decks_al_centro_no_saturan_con_el_master_por_defecto() {
    // Con master 0.9 y gain 0.8 el error contra la suma lineal ideal es -57.9 dB:
    // el limiter apenas roza. Con master 1.0 sube a -36.8 dB. Se fija el primero
    // como el caso sano y el segundo como deuda.
    let g = 0.8 * (PI / 4.0).cos();
    let suma_lineal = |master: f64| -> f64 {
        let p = Settings { gain: 0.8, crossfader: 0.5, master: master as f32, ..Default::default() };
        let y = render(
            vec![
                (0, sine(440.0, SOURCE_FRAMES, 0.9, SR as f64)),
                (1, sine(660.0, SOURCE_FRAMES, 0.9, SR as f64)),
            ],
            &p,
        );
        let (mut num, mut den) = (0.0f64, 0.0f64);
        for i in SKIP..RENDER_FRAMES {
            let t = i as f64 / SR as f64;
            let ideal = master
                * g as f64
                * ((2.0 * PI * 440.0 * t).sin() + (2.0 * PI * 660.0 * t).sin())
                * 0.9;
            let e = y[i * 2] as f64 - ideal;
            num += e * e;
            den += ideal * ideal;
        }
        10.0 * (num / den).log10()
    };
    let sano = suma_lineal(0.9);
    assert!(sano <= -45.0, "con master 0.9 el limiter casi no debe tocar (medido -57.9 dB), fue {sano:.1} dB");
    let al_techo = suma_lineal(1.0);
    assert!(al_techo <= -30.0, "con master 1.0 no debe empeorar de -36.8 dB, fue {al_techo:.1} dB");
}
