//! Sync por beatgrid: matemática pura, sin estado global y sin I/O.
//!
//! Convención única: posiciones en segundos del material original (tras el
//! remuestreo al SR del dispositivo, un frame es 1/SR s), tempos en BPM,
//! errores de fase en fracciones de beat normalizadas a [-0.5, +0.5].

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BeatGrid {
    pub bpm: f64,
    /// Posición del primer beat en segundos.
    pub first_beat: f64,
}

impl BeatGrid {
    pub fn new(bpm: f64, first_beat: f64) -> Self {
        Self { bpm, first_beat }
    }

    #[inline]
    pub fn beat_len(&self) -> f64 {
        60.0 / self.bpm
    }

    /// Fase dentro del beat, en [0, 1).
    #[inline]
    pub fn phase(&self, pos_s: f64) -> f64 {
        let beats = (pos_s - self.first_beat) / self.beat_len();
        beats.rem_euclid(1.0)
    }

    #[inline]
    pub fn beat_index(&self, pos_s: f64) -> i64 {
        ((pos_s - self.first_beat) / self.beat_len()).floor() as i64
    }

    /// Posición (s) del beat más cercano.
    pub fn nearest_beat(&self, pos_s: f64) -> f64 {
        self.first_beat + self.beat_index(pos_s) as f64 * self.beat_len()
    }
}

/// Error de fase del esclavo respecto al master, en beats, en [-0.5, +0.5).
/// Positivo = el esclavo va adelantado.
#[inline]
pub fn wrapped_error(master_phase: f64, slave_phase: f64) -> f64 {
    let mut d = slave_phase - master_phase;
    if d >= 0.5 {
        d -= 1.0;
    } else if d < -0.5 {
        d += 1.0;
    }
    d
}

/// Rate relativo para igualar tempo (1.0 = sin cambio), recortado al rango del
/// pitch fader. Si la relación cae fuera del rango pero está a menos de un 2 %
/// de un 2x/0.5x exacto, se propone la mitad o el doble en vez de rechazar.
pub fn tempo_rate(master_bpm: f64, slave_bpm: f64, max_rel: f64) -> Option<(f64, bool)> {
    if master_bpm <= 0.0 || slave_bpm <= 0.0 {
        return None;
    }
    let raw = master_bpm / slave_bpm;
    let half_double = if (raw - 2.0).abs() / 2.0 < 0.02 {
        Some(2.0)
    } else if (raw - 0.5).abs() / 0.5 < 0.02 {
        Some(0.5)
    } else {
        None
    };
    let candidate = raw / half_double.unwrap_or(1.0);
    if (candidate - 1.0).abs() <= max_rel {
        Some((candidate, half_double.is_some()))
    } else {
        None
    }
}

/// Corrección de rate para reducir el error de fase. Devuelve un offset
/// relativo (p. ej. -0.004 = 0.4 % más lento) que se suma a 1.0.
/// `deadband`: errores menores no se tocan (evita jitter audible).
pub fn phase_correction(err_beats: f64, deadband: f64, gain: f64, max_rel: f64) -> f64 {
    if err_beats.abs() <= deadband {
        return 0.0;
    }
    let wanted = -err_beats * gain;
    wanted.clamp(-max_rel, max_rel)
}

pub const DEFAULT_MAX_REL: f64 = 0.08;
pub const PHASE_DEADBAND: f64 = 0.004;
pub const PHASE_GAIN: f64 = 0.55;
pub const PHASE_MAX_REL: f64 = 0.03;

/// Un paso de sync: primero tempo, después fase. Devuelve el rate relativo a
/// aplicar al deck esclavo en este tick de control.
pub fn sync_relative_rate(
    master_pos_s: f64,
    master: BeatGrid,
    slave_pos_s: f64,
    slave: BeatGrid,
    max_rel: f64,
) -> (f64, f64) {
    let Some((tempo, _)) = tempo_rate(master.bpm, slave.bpm, max_rel) else {
        return (1.0, 0.0);
    };
    let err = wrapped_error(master.phase(master_pos_s), slave.phase(slave_pos_s));
    let corr = phase_correction(err, PHASE_DEADBAND, PHASE_GAIN, PHASE_MAX_REL);
    let rate = (tempo + corr).clamp(1.0 - max_rel, 1.0 + max_rel);
    (rate, err)
}
