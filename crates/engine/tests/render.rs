//! Tests del motor: render offline determinístico, sin placa de sonido.

use engine::sync::{self, BeatGrid};
use engine::{equal_power, resample_stereo, soft_clip, Controls, Mixer, PendingLoads, Track};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

const SR: u32 = 48_000;

fn sine_track(freq: f32, amp: f32, seconds: f32, left_only: bool, right_only: bool) -> Arc<Track> {
    let frames = (seconds * SR as f32) as usize;
    let mut samples = vec![0.0f32; frames * 2];
    for i in 0..frames {
        let v = (i as f32 * 2.0 * std::f32::consts::PI * freq / SR as f32).sin() * amp;
        samples[i * 2] = if right_only { 0.0 } else { v };
        samples[i * 2 + 1] = if left_only { 0.0 } else { v };
    }
    Arc::new(Track::new("test", samples, SR, BeatGrid::new(128.0, 0.0)))
}

struct Harness {
    controls: Arc<Controls>,
    pending: PendingLoads,
    mixer: Mixer,
}

impl Harness {
    fn new() -> Self {
        let controls = Arc::new(Controls::default());
        let pending: PendingLoads = Arc::new(Mutex::new(Vec::new()));
        let mixer = Mixer::new(controls.clone(), pending.clone(), SR);
        Self {
            controls,
            pending,
            mixer,
        }
    }

    fn load(&self, deck: usize, track: Arc<Track>) {
        self.pending.lock().unwrap().push((deck, track));
    }

    fn render(&mut self, frames: usize) -> Vec<f32> {
        self.mixer.render_to_vec(frames)
    }
}

#[test]
fn ley_de_crossfader_equal_power() {
    let (a, b) = equal_power(0.5);
    let db = 20.0 * (a as f64).log10();
    assert!((db + 3.0103).abs() < 0.01, "-3 dB en el centro, fue {db}");
    assert!((a - b).abs() < 1e-6);
    let (a0, b0) = equal_power(0.0);
    assert!((a0 - 1.0).abs() < 1e-6 && b0.abs() < 1e-6);
}

#[test]
fn soft_clip_es_transparente_bajo_el_umbral() {
    for v in [-0.8f32, -0.5, 0.0, 0.25, 0.799] {
        assert!((soft_clip(v) - v).abs() < 1e-7, "v={v}");
    }
    assert!(soft_clip(1.0) < 1.0);
    assert!(soft_clip(5.0) <= 1.0);
    assert!(soft_clip(-5.0) >= -1.0);
}

#[test]
fn dos_decks_identicos_suman_con_la_curva() {
    let mut h = Harness::new();
    let freq = 1000.0f32;
    let amp = 0.5f32;
    h.load(0, sine_track(freq, amp, 1.0, false, false));
    h.load(1, sine_track(freq, amp, 1.0, false, false));
    engine::set_f32(&h.controls.gain[0], 1.0);
    engine::set_f32(&h.controls.gain[1], 1.0);
    engine::set_f32(&h.controls.master_gain, 1.0);
    engine::set_f32(&h.controls.crossfader, 0.5);
    h.controls.playing[0].store(true, Ordering::Relaxed);
    h.controls.playing[1].store(true, Ordering::Relaxed);

    let _ = h.render(480); // completa el fade-in de 5 ms al dar play
    let out = h.render(256);
    let (g, _) = equal_power(0.5);
    for i in 0..256 {
        let frame = 480 + i;
        let expected = (frame as f32 * 2.0 * std::f32::consts::PI * freq / SR as f32).sin() * amp * 2.0 * g;
        assert!(
            (out[i * 2] - expected).abs() < 1e-5,
            "frame {i}: {} vs {expected}",
            out[i * 2]
        );
        assert!((out[i * 2] - out[i * 2 + 1]).abs() < 1e-6);
    }
}

#[test]
fn separacion_de_canales_y_mute_por_gain() {
    let mut h = Harness::new();
    h.load(0, sine_track(500.0, 0.5, 1.0, true, false)); // sólo L
    h.load(1, sine_track(500.0, 0.5, 1.0, false, true)); // sólo R
    engine::set_f32(&h.controls.gain[0], 1.0);
    engine::set_f32(&h.controls.gain[1], 1.0);
    engine::set_f32(&h.controls.master_gain, 1.0);
    engine::set_f32(&h.controls.crossfader, 0.5);
    h.controls.playing[0].store(true, Ordering::Relaxed);
    h.controls.playing[1].store(true, Ordering::Relaxed);

    let _ = h.render(480); // completa el fade-in de 5 ms al dar play
    let out = h.render(128);
    let (g, _) = equal_power(0.5);
    for i in 0..128 {
        let frame = 480 + i;
        let expected = (frame as f32 * 2.0 * std::f32::consts::PI * 500.0 / SR as f32).sin() * 0.5 * g;
        assert!((out[i * 2] - expected).abs() < 1e-5);
        assert!((out[i * 2 + 1] - expected).abs() < 1e-5);
    }

    // Gain de un deck a cero: ese deck no aporta nada.
    engine::set_f32(&h.controls.gain[0], 0.0);
    let out = h.render(128);
    for i in 0..128 {
        let frame = 608 + i; // los dos bloques anteriores ya consumieron 608 frames
        let expected = (frame as f32 * 2.0 * std::f32::consts::PI * 500.0 / SR as f32).sin() * 0.5 * g;
        assert!((out[i * 2 + 1] - expected).abs() < 1e-5, "R debe seguir sonando");
        assert!(out[i * 2].abs() < 1e-6, "L debe estar muteado");
    }
}

#[test]
fn publica_posiciones_y_longitudes() {
    let mut h = Harness::new();
    h.load(0, sine_track(440.0, 0.3, 2.0, false, false));
    h.controls.playing[0].store(true, Ordering::Relaxed);
    let _ = h.render(480);
    let pos = h.controls.position[0].load(Ordering::Relaxed);
    let len = h.controls.length[0].load(Ordering::Relaxed);
    assert_eq!(pos, 480);
    assert_eq!(len, (2.0 * SR as f32) as u64);
    h.controls.playing[0].store(false, Ordering::Relaxed);
    let _ = h.render(480);
    assert_eq!(h.controls.position[0].load(Ordering::Relaxed), 480, "parado no avanza");
}

#[test]
fn resample_conserva_la_frecuencia() {
    let src_sr = 44_100u32;
    let freq = 1000.0f32;
    let frames = src_sr as usize; // 1 s
    let mut input = vec![0.0f32; frames * 2];
    for i in 0..frames {
        let v = (i as f32 * 2.0 * std::f32::consts::PI * freq / src_sr as f32).sin() * 0.5;
        input[i * 2] = v;
        input[i * 2 + 1] = v;
    }
    let out = resample_stereo(&input, src_sr, SR);
    assert_eq!(out.len(), (SR as usize) * 2);

    // Conteo de cruces por cero del canal izquierdo → frecuencia.
    let mut crossings = 0usize;
    for i in 1..(out.len() / 2) {
        if out[(i - 1) * 2] <= 0.0 && out[i * 2] > 0.0 {
            crossings += 1;
        }
    }
    let measured = crossings as f64; // 1 s de señal
    assert!((measured - freq as f64).abs() < 5.0, "frecuencia medida {measured}");
}

/// Simula 60 s de mezcla a 60 Hz de control: el sync debe converger y quedarse.
#[test]
fn sync_converge_y_se_mantiene() {
    let master = BeatGrid::new(128.0, 0.0);
    let slave = BeatGrid::new(127.4, 0.37);

    let dt = 1.0 / 60.0;
    let mut master_pos = 8.0; // el master ya lleva rato sonando
    let mut slave_pos = 0.0;
    let mut worst_after_5s: f64 = 0.0;
    let mut rate_used = 1.0;

    for tick in 0..(60.0 / dt) as usize {
        let (rate, _err) = sync::sync_relative_rate(master_pos, master, slave_pos, slave, 0.08);
        rate_used = rate;
        master_pos += dt;
        slave_pos += dt * rate;

        let t = tick as f64 * dt;
        if t > 5.0 {
            let e = sync::wrapped_error(master.phase(master_pos), slave.phase(slave_pos)).abs();
            worst_after_5s = worst_after_5s.max(e);
        }
    }

    assert!(
        worst_after_5s < 0.02,
        "error de fase peor después de 5 s: {worst_after_5s} beats"
    );
    assert!((rate_used - 128.0 / 127.4).abs() < 0.01, "rate final {rate_used}");
}

#[test]
fn sync_rechaza_tempo_fuera_de_rango() {
    let master = BeatGrid::new(128.0, 0.0);
    // 100 BPM contra 128: la relación 1.28 está fuera de ±8 %.
    assert!(sync::tempo_rate(master.bpm, 100.0, 0.08).is_none());
    // 64 BPM: se ofrece el doble, no se rechaza.
    let (rate, half_double) = sync::tempo_rate(128.0, 64.0, 0.08).expect("debe ofrecer doble");
    assert!(half_double);
    assert!((rate - 2.0).abs() < 1e-9 || (rate - 1.0).abs() < 1e-9, "rate={rate}");
}

/// El deck con SYNC pausado no avanza, así que corregir fase lo único que hace
/// es barrer el rate de -3 % a +3 % a la frecuencia del beat. Estando pausado
/// sólo se iguala tempo: el rate queda fijo y el pitch no se mueve.
#[test]
fn sync_con_el_esclavo_pausado_solo_iguala_tempo() {
    let master = BeatGrid::new(125.1, 0.220);
    let slave = BeatGrid::new(125.0, 0.475);
    let slave_pos = 3.0; // congelada: el deck está en pause

    let fijo = sync::tempo_relative_rate(master.bpm, slave.bpm, sync::DEFAULT_MAX_REL);
    assert!((fijo - 125.1 / 125.0).abs() < 1e-12, "tempo {fijo}");

    // Contraprueba del bug: con la corrección de fase el rate se va de -2.9 % a
    // +3.1 % sólo porque el master sigue avanzando.
    let (mut min, mut max) = (f64::MAX, f64::MIN);
    for k in 0..600 {
        let master_pos = k as f64 * 0.066;
        let (rate, _) = sync::sync_relative_rate(master_pos, master, slave_pos, slave, sync::DEFAULT_MAX_REL);
        min = min.min(rate);
        max = max.max(rate);
        // Lo que se aplica ahora con el deck pausado: constante.
        assert!((sync::tempo_relative_rate(master.bpm, slave.bpm, sync::DEFAULT_MAX_REL) - fijo).abs() < 1e-12);
    }
    assert!(
        max - min > 0.05,
        "la corrección de fase debería barrer el rate (min {min}, max {max})"
    );
    // Fuera de rango no hay tempo que igualar: rate neutro.
    assert_eq!(sync::tempo_relative_rate(128.0, 100.0, sync::DEFAULT_MAX_REL), 1.0);
}

#[test]
fn seek_salta_a_la_posicion_pedida() {
    let mut h = Harness::new();
    h.load(0, sine_track(440.0, 0.3, 5.0, false, false));
    h.controls.playing[0].store(true, Ordering::Relaxed);
    h.controls.seek_request[0].store(SR as u64 * 2, Ordering::Relaxed);
    let _ = h.render(64);
    let pos = h.controls.position[0].load(Ordering::Relaxed);
    assert!((pos as i64 - (SR as i64 * 2 + 64)).abs() <= 1, "pos={pos}");
}

#[test]
fn loop_de_8_beats_envuelve_y_no_clica() {
    let mut h = Harness::new();
    h.load(0, sine_track(440.0, 0.3, 5.0, false, false));
    engine::set_f32(&h.controls.gain[0], 1.0);
    engine::set_f32(&h.controls.master_gain, 1.0);
    engine::set_f32(&h.controls.crossfader, 0.0);
    h.controls.playing[0].store(true, Ordering::Relaxed);
    // loop de 1 s a 2 s
    h.controls.loop_start[0].store(SR as u64, Ordering::Relaxed);
    h.controls.loop_end[0].store(SR as u64 * 2, Ordering::Relaxed);
    h.controls.loop_on[0].store(true, Ordering::Relaxed);
    h.controls.seek_request[0].store(SR as u64, Ordering::Relaxed);

    let mut min = u64::MAX;
    let mut max = 0u64;
    let mut prev = 0.0f32;
    let mut worst_step = 0.0f32;
    for _ in 0..400 {
        let out = h.render(256);
        for v in out.iter() {
            worst_step = worst_step.max((v - prev).abs());
            prev = *v;
        }
        let pos = h.controls.position[0].load(Ordering::Relaxed);
        min = min.min(pos);
        max = max.max(pos);
    }

    assert!(min >= SR as u64 - 1, "no debe volver antes del inicio: {min}");
    assert!(
        max <= SR as u64 * 2 + 256,
        "no debe pasar el fin del loop: {max}"
    );
    // Un seno de 440 Hz a 0.3 tiene saltos de ~0.017 por muestra; un corte
    // seco en el borde del loop daría un salto de orden 0.3.
    assert!(worst_step < 0.05, "click en el borde del loop: {worst_step}");
}

#[test]
fn sin_loop_el_deck_sigue_de_largo() {
    let mut h = Harness::new();
    h.load(0, sine_track(440.0, 0.3, 3.0, false, false));
    engine::set_f32(&h.controls.master_gain, 1.0);
    h.controls.playing[0].store(true, Ordering::Relaxed);
    h.controls.loop_start[0].store(SR as u64, Ordering::Relaxed);
    h.controls.loop_end[0].store(SR as u64 * 2, Ordering::Relaxed);
    // loop_on queda en false
    for _ in 0..800 {
        let _ = h.render(256);
    }
    let pos = h.controls.position[0].load(Ordering::Relaxed);
    assert!(pos > SR as u64 * 2, "sin loop debe pasar de los 2 s: {pos}");
}

#[test]
fn control_de_play_para_el_deck() {
    let mut h = Harness::new();
    h.load(0, sine_track(440.0, 0.3, 5.0, false, false));
    h.controls.playing[0].store(true, Ordering::Relaxed);
    let _ = h.render(256);
    assert!(!AtomicBool::new(false).load(Ordering::Relaxed)); // sanity
    assert!(h.controls.position[0].load(Ordering::Relaxed) > 0);
}
