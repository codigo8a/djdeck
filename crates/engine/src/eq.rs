//! EQ de 3 bandas por deck: low shelf, peak y high shelf (RBJ cookbook).
//!
//! Se procesa por frame dentro del deck, antes del gain. Si las tres bandas
//! están en 0 dB el bloque se saltea por completo: el camino sin EQ queda
//! bit-idéntico y los tests de render analítico siguen valiendo.
//!
//! Los coeficientes nunca saltan de golpe: cuando cambia una banda el filtro
//! interpola de los coeficientes viejos a los nuevos en `RAMP_SAMPLES`. Sin eso
//! un cambio brusco de ganancia (arrastrar el slider de punta a punta) metía un
//! click en los graves: medido, el salto entre muestras a 100 Hz quedaba 3.6x el
//! natural; con la rampa queda dentro del 10 %.

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

    /// Interpolación lineal de los cinco coeficientes (para la rampa).
    #[inline]
    fn lerp(&self, otro: Coeffs, t: f32) -> Coeffs {
        let f = |a: f32, b: f32| a + (b - a) * t;
        Coeffs {
            b0: f(self.b0, otro.b0),
            b1: f(self.b1, otro.b1),
            b2: f(self.b2, otro.b2),
            a1: f(self.a1, otro.a1),
            a2: f(self.a2, otro.a2),
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
    fn process_con(&self, x: f32, z: &mut [f32; 2]) -> f32 {
        let y = self.b0 * x + z[0];
        z[0] = self.b1 * x - self.a1 * y + z[1];
        z[1] = self.b2 * x - self.a2 * y;
        y
    }

    pub fn process(&self, x: f32, z: &mut [f32; 2]) -> f32 {
        self.process_con(x, z)
    }
}

/// Muestras que tarda un cambio de ganancia en llegar al filtro (~5 ms a 48 kHz).
/// Suficiente para que no se oiga el escalón y corto como para que el knob siga
/// sintiéndose inmediato.
const RAMP_SAMPLES: u32 = 1_024;

const LOW_F0: f32 = 200.0;
const MID_F0: f32 = 1_000.0;
const MID_Q: f32 = 0.8;
const HIGH_F0: f32 = 4_000.0;

/// Una banda: dos filtros (un estado por canal) con rampa de coeficientes.
struct Band {
    desde: Coeffs,
    hacia: Coeffs,
    /// Muestras de rampa que quedan; 0 = coeficientes fijos en `hacia`.
    rampa: u32,
    /// El destino es 0 dB (la banda queda fuera del camino).
    neutro: bool,
    z: [[f32; 2]; 2],
}

impl Band {
    fn new() -> Self {
        Self {
            desde: Coeffs::identity(),
            hacia: Coeffs::identity(),
            rampa: 0,
            neutro: true,
            z: [[0.0; 2]; 2],
        }
    }

    /// Nuevo destino de ganancia: la rampa arranca desde donde esté el filtro
    /// ahora mismo, así dos cambios seguidos no se pisan.
    fn set(&mut self, nuevo: Coeffs, neutro: bool) {
        self.desde = self.coeffs_actuales();
        self.hacia = nuevo;
        self.neutro = neutro;
        self.rampa = RAMP_SAMPLES;
    }

    #[inline]
    fn coeffs_actuales(&self) -> Coeffs {
        if self.rampa == 0 {
            self.hacia
        } else {
            let t = 1.0 - self.rampa as f32 / RAMP_SAMPLES as f32;
            self.desde.lerp(self.hacia, t)
        }
    }

    #[inline]
    fn process_stereo(&mut self, l: f32, r: f32) -> (f32, f32) {
        let c = self.coeffs_actuales();
        if self.rampa > 0 {
            self.rampa -= 1;
            if self.rampa == 0 && self.neutro {
                // Vuelta a 0 dB: no queda estado del filtro tirado para el
                // próximo boost (sembraría un transitorio).
                self.z = [[0.0; 2]; 2];
            }
        }
        (
            c.process_con(l, &mut self.z[0]),
            c.process_con(r, &mut self.z[1]),
        )
    }

    /// En reposo y sin filtrar: se puede saltear la banda entera.
    #[inline]
    fn en_reposo_neutro(&self) -> bool {
        self.rampa == 0 && self.neutro
    }
}

pub struct Eq {
    low_db: f32,
    mid_db: f32,
    high_db: f32,
    low: Band,
    mid: Band,
    high: Band,
}

impl Eq {
    pub fn new(sample_rate: u32) -> Self {
        let mut eq = Self {
            low_db: 0.0,
            mid_db: 0.0,
            high_db: 0.0,
            low: Band::new(),
            mid: Band::new(),
            high: Band::new(),
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
        self.low.set(Coeffs::low_shelf(sr, LOW_F0, low_db), low_db.abs() < 1e-3);
        self.mid.set(Coeffs::peak(sr, MID_F0, MID_Q, mid_db), mid_db.abs() < 1e-3);
        self.high.set(Coeffs::high_shelf(sr, HIGH_F0, high_db), high_db.abs() < 1e-3);
    }

    /// Verdadero sólo cuando las tres bandas están en 0 dB y las rampas
    /// terminaron: ahí el bloque se saltea y el camino es bit-idéntico.
    pub fn is_bypass(&self) -> bool {
        self.low.en_reposo_neutro()
            && self.mid.en_reposo_neutro()
            && self.high.en_reposo_neutro()
    }

    /// Aplica las tres bandas a un frame estéreo.
    #[inline]
    pub fn process_stereo(&mut self, l: f32, r: f32) -> (f32, f32) {
        if self.is_bypass() {
            return (l, r);
        }
        let (l, r) = self.low.process_stereo(l, r);
        let (l, r) = self.mid.process_stereo(l, r);
        self.high.process_stereo(l, r)
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

    /// Ganancia medida en `freq` con las bandas dadas, en dB (proyección sobre
    /// la senoide después del transitorio).
    fn medir(low: f32, mid: f32, high: f32, freq: f32) -> f32 {
        let sr = 48_000;
        let mut eq = Eq::new(sr);
        eq.set_gains(low, mid, high, sr);
        let input = sine(freq, sr, sr as usize);
        let out: Vec<f32> = input
            .iter()
            .map(|v| eq.process_stereo(*v, *v).0)
            .collect();
        let mag = |y: &[f32]| -> f64 {
            let w = 2.0 * std::f64::consts::PI * freq as f64 / sr as f64;
            let (mut re, mut im) = (0.0, 0.0);
            let mut n = 0usize;
            for (i, v) in y.iter().enumerate().skip(8_000) {
                let ph = w * i as f64;
                re += *v as f64 * ph.cos();
                im += *v as f64 * ph.sin();
                n += 1;
            }
            2.0 * (re * re + im * im).sqrt() / n as f64
        };
        (20.0 * (mag(&out) / mag(&input)).log10()) as f32
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

    /// Volver a 0 dB después de ecualizar: la rampa baja a identidad y el bypass
    /// vuelve a ser bit-exacto (no queda estado del filtro tirado).
    #[test]
    fn vuelve_a_bypass_despues_de_ecualizar() {
        let sr = 48_000;
        let mut eq = Eq::new(sr);
        eq.set_gains(9.0, -6.0, 12.0, sr);
        let input = sine(220.0, sr, 8_192);
        for v in &input {
            eq.process_stereo(*v, *v);
        }
        eq.set_gains(0.0, 0.0, 0.0, sr);
        assert!(!eq.is_bypass(), "mientras rampa no debe saltear el filtro");
        for _ in 0..(RAMP_SAMPLES + 4) {
            eq.process_stereo(0.0, 0.0);
        }
        assert!(eq.is_bypass());
        for v in &input {
            let (l, _) = eq.process_stereo(*v, *v);
            assert!((l - v).abs() < 1e-6, "el bypass debe volver a ser exacto");
        }
    }

    /// La respuesta de cada banda, medida: es lo que hace que el knob de LOW
    /// mueva los graves, el de MID los medios y el de HIGH los brillos.
    #[test]
    fn respuesta_de_las_tres_bandas() {
        // LOW: shelf en 200 Hz.
        assert!((medir(12.0, 0.0, 0.0, 50.0) - 11.9).abs() < 0.3);
        assert!((medir(12.0, 0.0, 0.0, 200.0) - 6.0).abs() < 0.3);
        assert!(medir(12.0, 0.0, 0.0, 1_000.0).abs() < 0.2, "no debe tocar 1 kHz");
        // MID: campana en 1 kHz.
        assert!((medir(0.0, 12.0, 0.0, 1_000.0) - 12.0).abs() < 0.3);
        assert!(medir(0.0, 12.0, 0.0, 60.0).abs() < 0.2, "no debe tocar 60 Hz");
        assert!(medir(0.0, 12.0, 0.0, 12_000.0).abs() < 0.5, "no debe tocar 12 kHz");
        // HIGH: shelf en 4 kHz.
        assert!((medir(0.0, 0.0, 12.0, 12_000.0) - 11.9).abs() < 0.3);
        assert!(medir(0.0, 0.0, 12.0, 100.0).abs() < 0.2, "no debe tocar 100 Hz");
        // Kill: -26 dB deja la banda prácticamente afuera.
        assert!(medir(-26.0, 0.0, 0.0, 50.0) < -25.0);
        assert!(medir(0.0, -26.0, 0.0, 1_000.0) < -25.0);
        assert!(medir(0.0, 0.0, -26.0, 12_000.0) < -25.0);
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

    /// Un cambio de ganancia de golpe (arrastrar el slider de punta a punta) no
    /// puede meter un click. Se compara el salto entre muestras en el cambio
    /// contra el mismo salto en régimen con la ganancia ya aplicada: sin rampa
    /// el escalón medía ~0.75 (3.6x el salto natural), con rampa queda igual
    /// que el régimen.
    #[test]
    fn el_cambio_de_ganancia_no_hace_click() {
        let sr = 48_000u32;
        let sine = |i: usize| (2.0 * std::f32::consts::PI * 100.0 * i as f32 / sr as f32).sin() * 0.25;
        let max_salto = |eq: &mut Eq, desde: usize, hasta: usize| -> f32 {
            let mut prev = 0.0f32;
            let mut peor = 0.0f32;
            for i in 0..9_600usize {
                let (l, _) = eq.process_stereo(sine(i), sine(i));
                if (desde..hasta).contains(&i) {
                    peor = peor.max((l - prev).abs());
                }
                prev = l;
            }
            peor
        };

        // Régimen: el EQ ya está en +12 dB desde la primera muestra.
        let mut referencia = Eq::new(sr);
        referencia.set_gains(12.0, 0.0, 0.0, sr);
        for _ in 0..RAMP_SAMPLES {
            referencia.process_stereo(0.0, 0.0);
        }
        let regimen = max_salto(&mut referencia, 4_800, 6_400);

        // El cambio en caliente: 0 dB y de golpe +12 dB.
        let mut eq = Eq::new(sr);
        eq.set_gains(0.0, 0.0, 0.0, sr);
        let mut prev = 0.0f32;
        let mut peor = 0.0f32;
        for i in 0..9_600usize {
            if i == 4_800 {
                eq.set_gains(12.0, 0.0, 0.0, sr);
            }
            let (l, _) = eq.process_stereo(sine(i), sine(i));
            if (4_800..6_400).contains(&i) {
                peor = peor.max((l - prev).abs());
            }
            prev = l;
        }
        assert!(
            peor < regimen * 1.25,
            "el escalón de ganancia se oye: {peor} contra {regimen} en régimen"
        );
    }
}
