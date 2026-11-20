//! Motor de audio de djdeck: decode, remuestreo, decks y mezcla.
//!
//! Reglas del MVP (y del resto del proyecto):
//! 1. El callback de audio no decodifica ni reserva memoria: sólo lee controles
//!    atómicos y hace multiplicaciones y sumas sobre buffers preasignados.
//! 2. La posición de un deck se cuenta en frames al SR del dispositivo, nunca en
//!    frames del archivo: así el sync tiene una sola unidad.
//! 3. Todo lo que suena se puede renderizar offline (`Mixer::render` sobre un
//!    buffer cualquiera) y por eso se puede testear sin placa de sonido.

pub mod decode;
pub mod eq;
pub mod output;
pub mod resample;
pub mod sync;

use anyhow::Result;
use resample::frame_at;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use sync::BeatGrid;

pub use decode::{decode_file, DecodedAudio};
pub use eq::Eq;
pub use resample::resample_stereo;

/// Mínimo audible de un beat a 120 BPM ≈ 4 ms; la deadband de fase es menor.
pub const DECK_COUNT: usize = 2;

// ---------------------------------------------------------------------------
// Controles compartidos UI <-> callback
// ---------------------------------------------------------------------------

/// Estado que la UI escribe y el callback lee. Sólo escalares atómicos: el hilo
/// de audio nunca espera un lock por un fader.
pub struct Controls {
    pub playing: [AtomicBool; DECK_COUNT],
    pub rate: [AtomicU32; DECK_COUNT],
    pub gain: [AtomicU32; DECK_COUNT],
    /// EQ de 3 bandas en dB (±12). 0 dB en las tres = camino sin EQ.
    pub eq_low: [AtomicU32; DECK_COUNT],
    pub eq_mid: [AtomicU32; DECK_COUNT],
    pub eq_high: [AtomicU32; DECK_COUNT],
    /// Posición actual en frames @ SR del dispositivo (la publica el callback).
    pub position: [AtomicU64; DECK_COUNT],
    pub length: [AtomicU64; DECK_COUNT],
    pub peak: [AtomicU32; DECK_COUNT],
    pub seek_request: [AtomicU64; DECK_COUNT],
    pub crossfader: AtomicU32,
    pub master_gain: AtomicU32,
    pub master_peak: AtomicU32,
    /// Total de frames que el callback realmente renderizó (para detectar
    /// que el dispositivo se come bloques).
    pub frames_rendered: AtomicU64,
    /// Tamaño del último bloque y el mayor visto (para reportar latencia real).
    pub block_frames_last: AtomicU64,
    pub block_frames_max: AtomicU64,
    pub xruns: AtomicU64,
}

impl Default for Controls {
    fn default() -> Self {
        Self {
            playing: [AtomicBool::new(false), AtomicBool::new(false)],
            rate: [AtomicU32::new(1.0f32.to_bits()), AtomicU32::new(1.0f32.to_bits())],
            gain: [AtomicU32::new(0.8f32.to_bits()), AtomicU32::new(0.8f32.to_bits())],
            eq_low: [AtomicU32::new(0), AtomicU32::new(0)],
            eq_mid: [AtomicU32::new(0), AtomicU32::new(0)],
            eq_high: [AtomicU32::new(0), AtomicU32::new(0)],
            position: [AtomicU64::new(0), AtomicU64::new(0)],
            length: [AtomicU64::new(0), AtomicU64::new(0)],
            peak: [AtomicU32::new(0), AtomicU32::new(0)],
            seek_request: [AtomicU64::new(u64::MAX), AtomicU64::new(u64::MAX)],
            crossfader: AtomicU32::new(0.5f32.to_bits()),
            master_gain: AtomicU32::new(0.9f32.to_bits()),
            master_peak: AtomicU32::new(0),
            frames_rendered: AtomicU64::new(0),
            block_frames_last: AtomicU64::new(0),
            block_frames_max: AtomicU64::new(0),
            xruns: AtomicU64::new(0),
        }
    }
}

#[inline]
pub fn get_f32(a: &AtomicU32) -> f32 {
    f32::from_bits(a.load(Ordering::Relaxed))
}

#[inline]
pub fn set_f32(a: &AtomicU32, v: f32) {
    a.store(v.to_bits(), Ordering::Relaxed);
}

/// Cola de cargas pendientes. Sólo la toca el callback con `try_lock`: si está
/// ocupada, el bloque actual ignora la carga y suena lo anterior.
pub type PendingLoads = Arc<Mutex<Vec<(usize, Arc<Track>)>>>;

// ---------------------------------------------------------------------------
// Track
// ---------------------------------------------------------------------------

pub struct Track {
    pub name: String,
    /// Estéreo entrelazado al SR del dispositivo.
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub grid: BeatGrid,
}

impl Track {
    pub fn new(name: impl Into<String>, samples: Vec<f32>, sample_rate: u32, grid: BeatGrid) -> Self {
        Self {
            name: name.into(),
            samples,
            sample_rate,
            grid,
        }
    }

    pub fn frames(&self) -> usize {
        self.samples.len() / 2
    }

    pub fn duration_s(&self) -> f64 {
        self.frames() as f64 / self.sample_rate as f64
    }

    /// Decodifica y remuestrea al SR pedido. La detección de BPM la hace el
    /// crate `analysis` (el motor no sabe de análisis).
    pub fn load(path: &Path, device_sr: u32, grid: BeatGrid) -> Result<Self> {
        let decoded = decode_file(path)?;
        let samples = resample_stereo(&decoded.samples, decoded.sample_rate, device_sr);
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("track")
            .to_string();
        Ok(Self::new(name, samples, device_sr, grid))
    }
}

// ---------------------------------------------------------------------------
// Mezcla
// ---------------------------------------------------------------------------

/// Curva equal-power: en el centro (-3.01 dB por canal) no hay bache de energía.
pub fn equal_power(x: f32) -> (f32, f32) {
    let x = x.clamp(0.0, 1.0);
    let a = (x * std::f32::consts::FRAC_PI_2).cos();
    let b = (x * std::f32::consts::FRAC_PI_2).sin();
    (a, b)
}

/// Limitador suave transparente por debajo de 0.8 y con techo en 1.0.
#[inline]
pub fn soft_clip(x: f32) -> f32 {
    const TH: f32 = 0.8;
    let a = x.abs();
    if a <= TH {
        x
    } else {
        let y = TH + (1.0 - TH) * ((a - TH) / (1.0 - TH)).tanh();
        if x.is_sign_negative() {
            -y
        } else {
            y
        }
    }
}

struct Deck {
    track: Option<Arc<Track>>,
    pos: f64,
    playing: bool,
    rate: f64,
    gain: f32,
    eq: Eq,
}

impl Deck {
    fn new(sample_rate: u32) -> Self {
        Self {
            track: None,
            pos: 0.0,
            playing: false,
            rate: 1.0,
            gain: 0.8,
            eq: Eq::new(sample_rate),
        }
    }

    fn seek(&mut self, frame: f64) {
        let max = self
            .track
            .as_ref()
            .map(|t| t.frames().saturating_sub(1) as f64)
            .unwrap_or(0.0);
        self.pos = frame.clamp(0.0, max);
    }

    /// Siguiente frame del deck, o `None` si está parado o terminó.
    #[inline]
    fn next_frame(&mut self) -> Option<(f32, f32)> {
        if !self.playing {
            return None;
        }
        let track = self.track.as_ref()?;
        let last = track.frames().saturating_sub(1) as f64;
        if self.pos >= last {
            self.playing = false;
            return None;
        }
        let (l, r) = frame_at(&track.samples, self.pos);
        self.pos += self.rate;
        Some(self.eq.process_stereo(l, r))
    }
}

pub struct Mixer {
    decks: [Deck; DECK_COUNT],
    controls: Arc<Controls>,
    pending: PendingLoads,
    sample_rate: u32,
}

impl Mixer {
    pub fn new(controls: Arc<Controls>, pending: PendingLoads, sample_rate: u32) -> Self {
        Self {
            decks: [Deck::new(sample_rate), Deck::new(sample_rate)],
            controls,
            pending,
            sample_rate,
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn poll_loads(&mut self) {
        let Ok(mut queue) = self.pending.try_lock() else {
            return;
        };
        for (idx, track) in queue.drain(..) {
            if idx >= DECK_COUNT {
                continue;
            }
            let frames = track.frames() as u64;
            self.decks[idx].track = Some(track);
            self.decks[idx].pos = 0.0;
            self.controls.length[idx].store(frames, Ordering::Relaxed);
            self.controls.position[idx].store(0, Ordering::Relaxed);
            // El estado de play lo decide quien controla: cargar un track no
            // debe pisar un play ya pedido.
        }
    }

    /// Renderiza `out.len()/2` frames y publica posiciones, longitudes y picos.
    pub fn render(&mut self, out: &mut [f32]) {
        if out.is_empty() {
            return;
        }
        self.poll_loads();

        let xf = get_f32(&self.controls.crossfader);
        let (g0, g1) = equal_power(xf);
        let master = get_f32(&self.controls.master_gain);

        for i in 0..DECK_COUNT {
            let deck = &mut self.decks[i];
            deck.playing = self.controls.playing[i].load(Ordering::Relaxed);
            let rate = get_f32(&self.controls.rate[i]) as f64;
            deck.rate = rate.clamp(1.0 - 0.5, 1.0 + 0.5);
            deck.gain = get_f32(&self.controls.gain[i]);
            let seek = self.controls.seek_request[i].swap(u64::MAX, Ordering::Relaxed);
            if seek != u64::MAX {
                deck.seek(seek as f64);
            }
            deck.eq.set_gains(
                get_f32(&self.controls.eq_low[i]),
                get_f32(&self.controls.eq_mid[i]),
                get_f32(&self.controls.eq_high[i]),
                self.sample_rate,
            );
        }

        let mut peaks = [0.0f32; DECK_COUNT];
        let mut master_peak = 0.0f32;

        let frames = out.len() / 2;
        self.controls
            .frames_rendered
            .fetch_add(frames as u64, Ordering::Relaxed);
        self.controls
            .block_frames_last
            .store(frames as u64, Ordering::Relaxed);
        self.controls
            .block_frames_max
            .fetch_max(frames as u64, Ordering::Relaxed);
        for f in 0..frames {
            let mut l = 0.0f32;
            let mut r = 0.0f32;
            for i in 0..DECK_COUNT {
                let xf_gain = if i == 0 { g0 } else { g1 };
                let g = self.decks[i].gain * xf_gain;
                if let Some((sl, sr)) = self.decks[i].next_frame() {
                    l += sl * g;
                    r += sr * g;
                    let p = sl.abs().max(sr.abs()) * g;
                    if p > peaks[i] {
                        peaks[i] = p;
                    }
                }
            }
            let ol = soft_clip(l * master);
            let or = soft_clip(r * master);
            master_peak = master_peak.max(ol.abs()).max(or.abs());
            out[f * 2] = ol;
            out[f * 2 + 1] = or;
        }

        for i in 0..DECK_COUNT {
            if self.decks[i].track.is_some() {
                set_f32(&self.controls.peak[i], peaks[i]);
                self.controls.position[i]
                    .store(self.decks[i].pos.max(0.0) as u64, Ordering::Relaxed);
                self.controls.playing[i].store(self.decks[i].playing, Ordering::Relaxed);
            }
        }
        set_f32(&self.controls.master_peak, master_peak);
    }

    /// Render offline a un vector, para tests y para exportar.
    pub fn render_to_vec(&mut self, frames: usize) -> Vec<f32> {
        let mut out = vec![0.0f32; frames * 2];
        self.render(&mut out);
        out
    }
}
