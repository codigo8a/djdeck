//! EQ de 3 bandas por deck: low shelf, peak y high shelf (RBJ cookbook).
//!
//! Se procesa por frame dentro del deck, antes del gain. Si las tres bandas
//! están en 0 dB el bloque se saltea por completo: el camino sin EQ queda
//! bit-idéntico y los tests de render analítico siguen valiendo.

#[derive(Clone, Copy)]
pub struct Coeffs {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
}

impl Coeffs {
    pub fn identity() -> Self {
        Self {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
        }
    }

    pub fn low_shelf(sr: f32, f0: f32, gain_db: f32) -> Self {
        let a = 10f32.powf(gain_db / 40.0);
        let w0 = 2.0 * std::f32::consts::PI * f0 / sr;
        let (sin, cos) = (w0.sin(), w0.cos());
        let alpha = sin * (2.0f32).sqrt() / 2.0;
        let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;
        let a0 = (a + 1.0) + (a - 1.0) * cos + two_sqrt_a_alpha;
        Self {
            b0: (a * ((a + 1.0) - (a - 1.0) * cos + two_sqrt_a_alpha)) / a0,
            b1: (2.0 * a * ((a - 1.0) - (a + 1.0) * cos)) / a0,
            b2: (a * ((a + 1.0) - (a - 1.0) * cos - two_sqrt_a_alpha)) / a0,
            a1: (-2.0 * ((a - 1.0) + (a + 1.0) * cos)) / a0,
            a2: ((a + 1.0) + (a - 1.0) * cos - two_sqrt_a_alpha) / a0,
        }
    }

    pub fn high_shelf(sr: f32, f0: f32, gain_db: f32) -> Self {
        let a = 10f32.powf(gain_db / 40.0);
        let w0 = 2.0 * std::f32::consts::PI * f0 / sr;
        let (sin, cos) = (w0.sin(), w0.cos());
        let alpha = sin * (2.0f32).sqrt() / 2.0;
        let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;
        let a0 = (a + 1.0) - (a - 1.0) * cos + two_sqrt_a_alpha;
        Self {
            b0: (a * ((a + 1.0) + (a - 1.0) * cos + two_sqrt_a_alpha)) / a0,
            b1: (-2.0 * a * ((a - 1.0) + (a + 1.0) * cos)) / a0,
            b2: (a * ((a + 1.0) + (a - 1.0) * cos - two_sqrt_a_alpha)) / a0,
            a1: (2.0 * ((a - 1.0) - (a + 1.0) * cos)) / a0,
            a2: ((a + 1.0) - (a - 1.0) * cos - two_sqrt_a_alpha) / a0,
        }
    }

    /// Campana centrada en f0 con Q dado (el "medio" del mixer).
    pub fn peak(sr: f32, f0: f32, q: f32, gain_db: f32) -> Self {
        let a = 10f32.powf(gain_db / 40.0);
        let w0 = 2.0 * std::f32::consts::PI * f0 / sr;
        let (sin, cos) = (w0.sin(), w0.cos());
        let alpha = sin / (2.0 * q);
        let a0 = 1.0 + alpha / a;
        Self {
            b0: (1.0 + alpha * a) / a0,
            b1: (-2.0 * cos) / a0,
            b2: (1.0 - alpha * a) / a0,
            a1: (-2.0 * cos) / a0,
            a2: (1.0 - alpha / a) / a0,
        }
    }

    /// Procesa una muestra con estado (forma directa II transpuesta).
    #[inline]
    pub fn process(&self, x: f32, z: &mut [f32; 2]) -> f32 {
        let y = self.b0 * x + z[0];
        z[0] = self.b1 * x - self.a1 * y + z[1];
        z[1] = self.b2 * x - self.a2 * y;
        y
    }
}

const LOW_F0: f32 = 200.0;
const MID_F0: f32 = 1_000.0;
const MID_Q: f32 = 0.8;
const HIGH_F0: f32 = 4_000.0;

pub struct Eq {
    low_db: f32,
    mid_db: f32,
    high_db: f32,
    low: Coeffs,
    mid: Coeffs,
    high: Coeffs,
    /// Estado por canal para cada banda.
    z_low: [[f32; 2]; 2],
    z_mid: [[f32; 2]; 2],
    z_high: [[f32; 2]; 2],
    bypass: bool,
}

impl Eq {
    pub fn new(sample_rate: u32) -> Self {
        let mut eq = Self {
            low_db: 0.0,
            mid_db: 0.0,
            high_db: 0.0,
            low: Coeffs::identity(),
            mid: Coeffs::identity(),
            high: Coeffs::identity(),
            z_low: [[0.0; 2]; 2],
            z_mid: [[0.0; 2]; 2],
            z_high: [[0.0; 2]; 2],
            bypass: true,
        };
        eq.set_gains(0.0, 0.0, 0.0, sample_rate);
        eq
    }

    /// Recalcula coeficientes sólo si algún valor cambió (control rate).
    pub fn set_gains(&mut self, low_db: f32, mid_db: f32, high_db: f32, sample_rate: u32) {
        if (low_db - self.low_db).abs() < 1e-3
            && (mid_db - self.mid_db).abs() < 1e-3
            && (high_db - self.high_db).abs() < 1e-3
        {
            return;
        }
        self.low_db = low_db;
        self.mid_db = mid_db;
        self.high_db = high_db;
        let sr = sample_rate as f32;
        self.bypass = low_db.abs() < 1e-3 && mid_db.abs() < 1e-3 && high_db.abs() < 1e-3;
        self.low = Coeffs::low_shelf(sr, LOW_F0, low_db);
        self.mid = Coeffs::peak(sr, MID_F0, MID_Q, mid_db);
        self.high = Coeffs::high_shelf(sr, HIGH_F0, high_db);
    }

    pub fn is_bypass(&self) -> bool {
        self.bypass
    }

    /// Aplica las tres bandas a un frame estéreo.
    #[inline]
    pub fn process_stereo(&mut self, l: f32, r: f32) -> (f32, f32) {
        if self.bypass {
            return (l, r);
        }
        let mut out = [l, r];
        for (ch, slot) in out.iter_mut().enumerate() {
            let mut v = *slot;
            v = self.low.process(v, &mut self.z_low[ch]);
            v = self.mid.process(v, &mut self.z_mid[ch]);
            v = self.high.process(v, &mut self.z_high[ch]);
            *slot = v;
        }
        (out[0], out[1])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f32, sr: u32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| (i as f32 * 2.0 * std::f32::consts::PI * freq / sr as f32).sin() * 0.5)
            .collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
    }

    /// Con las tres bandas en 0 dB el EQ no toca la señal.
    #[test]
    fn identidad_en_0db() {
        let sr = 48_000;
        let mut eq = Eq::new(sr);
        assert!(eq.is_bypass());
        let input = sine(440.0, sr, 4096);
        for v in &input {
            let (l, _) = eq.process_stereo(*v, *v);
            assert!((l - v).abs() < 1e-9, "el bypass debe ser exacto");
        }
    }

    #[test]
    fn shelf_bajo_sube_los_graves() {
        let sr = 48_000;
        let mut eq = Eq::new(sr);
        eq.set_gains(12.0, 0.0, 0.0, sr);
        assert!(!eq.is_bypass());
        let input = sine(100.0, sr, 48_000);
        let out: Vec<f32> = input
            .iter()
            .map(|v| eq.process_stereo(*v, *v).0)
            .collect();
        // +12 dB = 4x en la banda del shelf; se mide después del transitorio.
        let ratio = rms(&out[4_000..]) / rms(&input[4_000..]);
        assert!((3.0..4.3).contains(&ratio), "ratio {ratio}");
    }

    #[test]
    fn shelf_alto_corta_los_agudos() {
        let sr = 48_000;
        let mut eq = Eq::new(sr);
        eq.set_gains(0.0, 0.0, -12.0, sr);
        let input = sine(10_000.0, sr, 48_000);
        let out: Vec<f32> = input
            .iter()
            .map(|v| eq.process_stereo(*v, *v).0)
            .collect();
        let ratio = rms(&out[4_000..]) / rms(&input[4_000..]);
        assert!(ratio < 0.4, "ratio {ratio}");
    }

    #[test]
    fn peak_medio_no_toca_los_extremos() {
        let sr = 48_000;
        let mut eq = Eq::new(sr);
        eq.set_gains(0.0, 12.0, 0.0, sr);
        let graves = sine(60.0, sr, 48_000);
        let out: Vec<f32> = graves
            .iter()
            .map(|v| eq.process_stereo(*v, *v).0)
            .collect();
        let ratio = rms(&out[4_000..]) / rms(&graves[4_000..]);
        assert!((0.8..1.2).contains(&ratio), "un peak de 1 kHz no debe mover 60 Hz: {ratio}");
    }
}
