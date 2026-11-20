//! djdeck — MVP: dos decks, mezcla y sync.
//!
//! Uso:
//!   djdeck "track-a.mp3" "track-b.flac"     # carga dos archivos
//!   djdeck                                  # arranca con dos demos sintéticos

use analysis::Peaks;
use anyhow::{Context, Result};
use engine::sync::{self, BeatGrid};
use engine::{Controls, PendingLoads, Track, DECK_COUNT};
use slint::{Image, Rgba8Pixel, SharedPixelBuffer, Timer, TimerMode};
use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

slint::slint! {
    import { Button, Slider } from "std-widgets.slint";

    component Meter inherits Rectangle {
        in property <float> level: 0.0;
        in property <color> accent: #22d3ee;
        height: 6px;
        background: #0a0d11;
        border-radius: 3px;
        Rectangle {
            x: 0;
            width: parent.width * Math.min(1.0, root.level);
            height: parent.height;
            background: root.accent;
            border-radius: 3px;
        }
    }

    component Deck inherits Rectangle {
        in property <string> label: "A";
        in property <string> track-name: "sin cargar";
        in property <string> bpm-text: "—";
        in property <string> time-text: "0:00";
        in property <string> pitch-text: "0.0%";
        in property <string> gain-text: "80%";
        in property <bool> playing: false;
        in property <bool> synced: false;
        in property <image> wave;
        in property <float> head: 0.0;
        in property <float> meter: 0.0;
        in-out property <float> pitch-pct: 0.0;
        in-out property <float> gain: 0.8;
        in property <color> accent: #22d3ee;

        callback play();
        callback cue();
        callback sync();
        callback pitch-changed(float);
        callback gain-changed(float);

        background: #151a21;
        border-radius: 12px;
        border-width: 1px;
        border-color: #232c37;

        VerticalLayout {
            padding: 14px;
            spacing: 10px;

            HorizontalLayout {
                spacing: 10px;
                Text {
                    text: root.label;
                    color: root.accent;
                    font-size: 20px;
                    font-weight: 800;
                }
                Text {
                    text: root.track-name;
                    color: #dbe4ee;
                    font-size: 13px;
                    overflow: elide;
                    vertical-alignment: center;
                }
                Rectangle { horizontal-stretch: 1; }
                Text {
                    text: root.bpm-text;
                    color: #8b98a8;
                    font-size: 13px;
                    vertical-alignment: center;
                }
                Text {
                    text: root.time-text;
                    color: #8b98a8;
                    font-size: 13px;
                    vertical-alignment: center;
                }
            }

            wf := Rectangle {
                min-height: 120px;
                vertical-stretch: 1;
                background: #080b0f;
                border-radius: 8px;
                clip: true;
                Image {
                    source: root.wave;
                    width: 100%;
                    height: 100%;
                }
                Rectangle {
                    x: (wf.width - 2px) * root.head;
                    width: 2px;
                    height: wf.height;
                    background: white;
                }
            }

            HorizontalLayout {
                spacing: 8px;
                Button {
                    text: root.playing ? "PAUSA" : "PLAY";
                    clicked => { root.play(); }
                }
                Button {
                    text: "CUE";
                    clicked => { root.cue(); }
                }
                Button {
                    text: root.synced ? "SYNC ON" : "SYNC";
                    clicked => { root.sync(); }
                }
                Rectangle { horizontal-stretch: 1; }
                Meter { level: root.meter; accent: root.accent; width: 90px; }
            }

            HorizontalLayout {
                spacing: 8px;
                Text {
                    text: "PITCH";
                    color: #6b7787;
                    font-size: 11px;
                    width: 42px;
                    vertical-alignment: center;
                }
                Slider {
                    minimum: -8.0;
                    maximum: 8.0;
                    value: root.pitch-pct;
                    changed(v) => { root.pitch-pct = v; root.pitch-changed(v); }
                }
                Text {
                    text: root.pitch-text;
                    color: #dbe4ee;
                    font-size: 12px;
                    width: 52px;
                    horizontal-alignment: right;
                    vertical-alignment: center;
                }
            }

            HorizontalLayout {
                spacing: 8px;
                Text {
                    text: "GAIN";
                    color: #6b7787;
                    font-size: 11px;
                    width: 42px;
                    vertical-alignment: center;
                }
                Slider {
                    minimum: 0.0;
                    maximum: 1.5;
                    value: root.gain;
                    changed(v) => { root.gain = v; root.gain-changed(v); }
                }
                Text {
                    text: root.gain-text;
                    color: #dbe4ee;
                    font-size: 12px;
                    width: 52px;
                    horizontal-alignment: right;
                    vertical-alignment: center;
                }
            }
        }
    }

    export component AppWindow inherits Window {
        title: "djdeck";
        width: 1200px;
        height: 640px;
        background: #0b0f14;

        in-out property <string> name-a: "sin cargar";
        in-out property <string> name-b: "sin cargar";
        in-out property <string> bpm-a: "—";
        in-out property <string> bpm-b: "—";
        in-out property <string> pitch-a-text: "0.0%";
        in-out property <string> pitch-b-text: "0.0%";
        in-out property <string> gain-a-text: "80%";
        in-out property <string> gain-b-text: "80%";
        in-out property <string> time-a: "0:00";
        in-out property <string> time-b: "0:00";
        in-out property <bool> playing-a: false;
        in-out property <bool> playing-b: false;
        in-out property <bool> synced-a: false;
        in-out property <bool> synced-b: false;
        in-out property <float> head-a: 0.0;
        in-out property <float> head-b: 0.0;
        in-out property <float> meter-a: 0.0;
        in-out property <float> meter-b: 0.0;
        in-out property <float> pitch-a: 0.0;
        in-out property <float> pitch-b: 0.0;
        in-out property <float> gain-a: 0.8;
        in-out property <float> gain-b: 0.8;
        in-out property <float> xf: 0.5;
        in-out property <image> wave-a;
        in-out property <image> wave-b;
        in-out property <string> status: "";

        callback play-a();
        callback play-b();
        callback cue-a();
        callback cue-b();
        callback sync-a();
        callback sync-b();
        callback pitch-a-changed(float);
        callback pitch-b-changed(float);
        callback gain-a-changed(float);
        callback gain-b-changed(float);
        callback xf-changed(float);

        VerticalLayout {
            padding: 16px;
            spacing: 14px;

            HorizontalLayout {
                spacing: 14px;

                Deck {
                    label: "A";
                    accent: #22d3ee;
                    vertical-stretch: 1;
                    track-name: root.name-a;
                    bpm-text: root.bpm-a;
                    pitch-text: root.pitch-a-text;
                    gain-text: root.gain-a-text;
                    time-text: root.time-a;
                    playing: root.playing-a;
                    synced: root.synced-a;
                    head: root.head-a;
                    meter: root.meter-a;
                    pitch-pct: root.pitch-a;
                    gain: root.gain-a;
                    wave: root.wave-a;
                    play => { root.play-a(); }
                    cue => { root.cue-a(); }
                    sync => { root.sync-a(); }
                    pitch-changed(v) => { root.pitch-a = v; root.pitch-a-changed(v); }
                    gain-changed(v) => { root.gain-a = v; root.gain-a-changed(v); }
                }

                Deck {
                    label: "B";
                    accent: #fbbf24;
                    vertical-stretch: 1;
                    track-name: root.name-b;
                    bpm-text: root.bpm-b;
                    pitch-text: root.pitch-b-text;
                    gain-text: root.gain-b-text;
                    time-text: root.time-b;
                    playing: root.playing-b;
                    synced: root.synced-b;
                    head: root.head-b;
                    meter: root.meter-b;
                    pitch-pct: root.pitch-b;
                    gain: root.gain-b;
                    wave: root.wave-b;
                    play => { root.play-b(); }
                    cue => { root.cue-b(); }
                    sync => { root.sync-b(); }
                    pitch-changed(v) => { root.pitch-b = v; root.pitch-b-changed(v); }
                    gain-changed(v) => { root.gain-b = v; root.gain-b-changed(v); }
                }
            }

            HorizontalLayout {
                alignment: center;
                spacing: 14px;
                Text { text: "A"; color: #22d3ee; font-size: 16px; font-weight: 800; vertical-alignment: center; }
                Slider {
                    width: 55%;
                    minimum: 0.0;
                    maximum: 1.0;
                    value: root.xf;
                    changed(v) => { root.xf = v; root.xf-changed(v); }
                }
                Text { text: "B"; color: #fbbf24; font-size: 16px; font-weight: 800; vertical-alignment: center; }
            }

            Text {
                text: root.status;
                color: #6b7787;
                font-size: 12px;
                horizontal-alignment: center;
            }
        }
    }
}

/// 1024 frames ≈ 21 ms de buffer: medido estable en PipeWire (256 frames produce
/// underruns sostenidos y el reloj de audio se atrasa ~16 %). Se puede forzar con
/// `DJDECK_FRAMES`.
const DEFAULT_FRAMES: u32 = 1024;

/// Estado de la UI que no vive en los controles atómicos.
struct DeckState {
    track: Option<Arc<Track>>,
    syncing: bool,
    pitch_pct: f64,
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--selftest") {
        return run_selftest();
    }

    if let Some(arg) = args.iter().find(|a| a.starts_with("--autoplay")) {
        let secs: u64 = arg
            .split('=')
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(10);
        return run_device_test(secs);
    }

    let controls = Arc::new(Controls::default());
    let pending: PendingLoads = Arc::new(Mutex::new(Vec::new()));

    let out = engine::output::start(controls.clone(), pending.clone(), DEFAULT_FRAMES)?;
    let sr = out.sample_rate;
    let latency_ms = out.buffer_frames as f64 * 1000.0 / sr as f64;
    println!(
        "dispositivo: {} | {} Hz | {} canales | {} frames ({:.1} ms)",
        out.device_name, sr, out.channels, out.buffer_frames, latency_ms
    );

    let decks = Rc::new(RefCell::new([
        DeckState {
            track: None,
            syncing: false,
            pitch_pct: 0.0,
        },
        DeckState {
            track: None,
            syncing: false,
            pitch_pct: 0.0,
        },
    ]));

    let ui = AppWindow::new()?;

    // Carga inicial: archivos de la CLI o dos demos sintéticos.
    let mut loaded: [Option<Arc<Track>>; DECK_COUNT] = [None, None];
    if args.is_empty() {
        for i in 0..DECK_COUNT {
            let bpm = if i == 0 { 127.4 } else { 128.0 };
            let t = Arc::new(make_demo_track(bpm, sr, if i == 0 { -3.0 } else { 0.0 }));
            loaded[i] = Some(t);
        }
        println!("sin archivos: uso dos demos sintéticos (127.4 y 128.0 BPM)");
    } else {
        for (i, path) in args.iter().take(DECK_COUNT).enumerate() {
            let p = Path::new(path);
            print!("analizando {} … ", p.display());
            let decoded = engine::decode_file(p).with_context(|| format!("leyendo {path}"))?;
            let samples = engine::resample_stereo(&decoded.samples, decoded.sample_rate, sr);
            let (bpm, first) = analysis::detect_bpm(&samples, sr)
                .ok_or_else(|| anyhow::anyhow!("no se pudo detectar el BPM de {path}"))?;
            let name = p.file_stem().and_then(|s| s.to_str()).unwrap_or("track");
            let track = Track::new(name, samples, sr, BeatGrid::new(bpm, first));
            println!("{bpm:.1} BPM, primer beat {first:.3}s");
            loaded[i] = Some(Arc::new(track));
        }
    }

    for i in 0..DECK_COUNT {
        if let Some(track) = loaded[i].clone() {
            let peaks = analysis::peaks(&track.samples, 900);
            let grid = track.grid;
            let name = track.name.clone();
            pending.lock().unwrap().push((i, track.clone()));
            decks.borrow_mut()[i].track = Some(track);

            let wave = wave_image(&peaks, 900, 104, accent(i));
            if i == 0 {
                ui.set_wave_a(wave);
                ui.set_name_a(name.into());
                ui.set_bpm_a(format!("{:.1} BPM", grid.bpm).into());
            } else {
                ui.set_wave_b(wave);
                ui.set_name_b(name.into());
                ui.set_bpm_b(format!("{:.1} BPM", grid.bpm).into());
            }
        }
    }

    // ---- callbacks de UI ----
    {
        let controls = controls.clone();
        ui.on_play_a(move || toggle_play(&controls, 0));
    }
    {
        let controls = controls.clone();
        ui.on_play_b(move || toggle_play(&controls, 1));
    }
    {
        let controls = controls.clone();
        let decks = decks.clone();
        ui.on_cue_a(move || cue(&controls, &decks, 0));
    }
    {
        let controls = controls.clone();
        let decks = decks.clone();
        ui.on_cue_b(move || cue(&controls, &decks, 1));
    }
    {
        let decks = decks.clone();
        let ui_weak = ui.as_weak();
        ui.on_sync_a(move || toggle_sync(&decks, &ui_weak, 0));
    }
    {
        let decks = decks.clone();
        let ui_weak = ui.as_weak();
        ui.on_sync_b(move || toggle_sync(&decks, &ui_weak, 1));
    }
    {
        let controls = controls.clone();
        let decks = decks.clone();
        ui.on_pitch_a_changed(move |v| set_pitch(&controls, &decks, 0, v));
    }
    {
        let controls = controls.clone();
        let decks = decks.clone();
        ui.on_pitch_b_changed(move |v| set_pitch(&controls, &decks, 1, v));
    }
    {
        let controls = controls.clone();
        ui.on_gain_a_changed(move |v| engine::set_f32(&controls.gain[0], v));
    }
    {
        let controls = controls.clone();
        ui.on_gain_b_changed(move |v| engine::set_f32(&controls.gain[1], v));
    }
    {
        let controls = controls.clone();
        ui.on_xf_changed(move |v| engine::set_f32(&controls.crossfader, v));
    }

    ui.set_gain_a(engine::get_f32(&controls.gain[0]));
    ui.set_gain_b(engine::get_f32(&controls.gain[1]));
    ui.set_xf(engine::get_f32(&controls.crossfader));
    ui.set_status(
        format!(
            "{} Hz · {} frames ({:.1} ms de buffer) · SYNC alinea tempo y fase contra el otro deck",
            sr, out.buffer_frames, latency_ms
        )
        .into(),
    );

    // ---- refresco a 20 Hz: posiciones, medidores y lazo de sync ----
    {
        let controls = controls.clone();
        let decks = decks.clone();
        let ui_weak = ui.as_weak();
        let sr = sr as f64;
        let timer = Timer::default();
        timer.start(TimerMode::Repeated, Duration::from_millis(50), move || {
            let Some(ui) = ui_weak.upgrade() else { return };

            let pos: Vec<f64> = (0..DECK_COUNT)
                .map(|i| controls.position[i].load(Ordering::Relaxed) as f64 / sr)
                .collect();
            let len: Vec<f64> = (0..DECK_COUNT)
                .map(|i| controls.length[i].load(Ordering::Relaxed) as f64 / sr)
                .collect();

            // Lazo de sync: el deck sincronizado sigue al otro.
            for i in 0..DECK_COUNT {
                let other = 1 - i;
                let (want, my_grid, other_grid) = {
                    let st = decks.borrow();
                    (
                        st[i].syncing && st[other].track.is_some(),
                        st[i].track.as_ref().map(|t| t.grid),
                        st[other].track.as_ref().map(|t| t.grid),
                    )
                };
                if let (true, Some(my_grid), Some(other_grid)) = (want, my_grid, other_grid) {
                    let (rate, _err) =
                        sync::sync_relative_rate(pos[other], other_grid, pos[i], my_grid, 0.08);
                    engine::set_f32(&controls.rate[i], rate as f32);
                }
            }

            let peaks = [
                engine::get_f32(&controls.peak[0]),
                engine::get_f32(&controls.peak[1]),
            ];
            let playing = [
                controls.playing[0].load(Ordering::Relaxed),
                controls.playing[1].load(Ordering::Relaxed),
            ];

            ui.set_head_a(fraction(pos[0], len[0]) as f32);
            ui.set_head_b(fraction(pos[1], len[1]) as f32);
            ui.set_time_a(fmt_time(pos[0], len[0]).into());
            ui.set_time_b(fmt_time(pos[1], len[1]).into());
            ui.set_meter_a((peaks[0] * 6.0).min(1.0));
            ui.set_meter_b((peaks[1] * 6.0).min(1.0));
            ui.set_playing_a(playing[0]);
            ui.set_playing_b(playing[1]);

            let pitch = {
                let st = decks.borrow();
                [st[0].pitch_pct, st[1].pitch_pct]
            };
            ui.set_pitch_a_text(format!("{:+.1}%", pitch[0]).into());
            ui.set_pitch_b_text(format!("{:+.1}%", pitch[1]).into());

            let gains = [
                engine::get_f32(&controls.gain[0]),
                engine::get_f32(&controls.gain[1]),
            ];
            ui.set_gain_a_text(format!("{:.0}%", gains[0] * 100.0).into());
            ui.set_gain_b_text(format!("{:.0}%", gains[1] * 100.0).into());
        });
        std::mem::forget(timer); // vive lo que vive la app
    }

    ui.run()?;
    drop(out);
    Ok(())
}

/// Verificación sin UI ni placa de sonido: dos demos, deck B sincronizado a A,
/// 30 s de mezcla en bloques de 256 frames con lazo de control cada ~48 ms.
/// Imprime el BPM detectado y el peor error de fase después de 5 s.
fn run_selftest() -> Result<()> {
    let sr = 48_000u32;
    let controls = Arc::new(Controls::default());
    let pending: PendingLoads = Arc::new(Mutex::new(Vec::new()));
    let mut mixer = engine::Mixer::new(controls.clone(), pending.clone(), sr);

    let a = Arc::new(make_demo_track(127.4, sr, -3.0));
    let b = Arc::new(make_demo_track(128.0, sr, 0.0));
    let (grid_a, grid_b) = (a.grid, b.grid);
    let grid_a = BeatGrid::new(grid_a.bpm, 0.0);
    let grid_b = BeatGrid::new(grid_b.bpm, 0.0);

    for (name, track) in [("A", &a), ("B", &b)] {
        let (bpm, first) = analysis::detect_bpm(&track.samples, sr).unwrap_or((0.0, 0.0));
        println!(
            "demo {name}: real {:.1} BPM / detectado {:.2} BPM / fase {:.3}s",
            track.grid.bpm, bpm, first
        );
    }

    pending.lock().unwrap().push((0, a));
    pending.lock().unwrap().push((1, b));
    engine::set_f32(&controls.gain[0], 1.0);
    engine::set_f32(&controls.gain[1], 1.0);
    engine::set_f32(&controls.master_gain, 1.0);
    engine::set_f32(&controls.crossfader, 0.5);
    controls.playing[0].store(true, Ordering::Relaxed);
    controls.playing[1].store(true, Ordering::Relaxed);

    const BLOCK: usize = 256;
    let blocks_per_tick = 9; // 9 * 256 / 48000 ≈ 48 ms
    let mut peak = 0.0f32;
    let mut worst_after_5s = 0.0f64;
    let mut final_err = 0.0f64;
    let mut buf = vec![0.0f32; BLOCK * 2];
    let seconds = 30.0;
    let ticks = (seconds * sr as f64 / (BLOCK * blocks_per_tick) as f64) as usize;

    for tick in 0..ticks {
        for _ in 0..blocks_per_tick {
            mixer.render(&mut buf);
            for v in buf.iter() {
                peak = peak.max(v.abs());
            }
            let pos_a = controls.position[0].load(Ordering::Relaxed) as f64 / sr as f64;
            let pos_b = controls.position[1].load(Ordering::Relaxed) as f64 / sr as f64;
            let (rate, err) = sync::sync_relative_rate(pos_a, grid_a, pos_b, grid_b, 0.08);
            engine::set_f32(&controls.rate[1], rate as f32);
            final_err = err;
            if tick as f64 * (BLOCK * blocks_per_tick) as f64 / sr as f64 > 5.0 {
                worst_after_5s = worst_after_5s.max(err.abs());
            }
        }
    }

    println!(
        "selftest: {} s mezclados | pico master {:.3} | peor error de fase tras 5 s {:.4} beats | final {:.4}",
        seconds, peak, worst_after_5s, final_err
    );

    let mut ok = true;
    if worst_after_5s >= 0.02 {
        println!("FALLO: el error de fase supera 0.02 beats");
        ok = false;
    }
    if peak >= 0.999 {
        println!("FALLO: la mezcla satura");
        ok = false;
    }
    if peak < 0.05 {
        println!("FALLO: no hay señal en la mezcla");
        ok = false;
    }
    if ok {
        println!("OK: sync convergente, mezcla sin saturar");
        Ok(())
    } else {
        std::process::exit(1)
    }
}

/// Verificación de punta a punta con la placa real, sin UI: abre el dispositivo,
/// pone los dos demos a sonar, sincroniza B contra A y reporta pico, posiciones
/// y xruns. Sirve para saber si el camino de audio vive o no.
fn run_device_test(secs: u64) -> Result<()> {
    let controls = Arc::new(Controls::default());
    let pending: PendingLoads = Arc::new(Mutex::new(Vec::new()));
    let out = engine::output::start(controls.clone(), pending.clone(), DEFAULT_FRAMES)?;
    let sr = out.sample_rate;
    println!(
        "autoplay: {} | {} Hz | buffer del backend | pedido 0 frames",
        out.device_name, sr
    );
    let a = Arc::new(make_demo_track(127.4, sr, -3.0));
    let b = Arc::new(make_demo_track(128.0, sr, 0.0));
    let (grid_a, grid_b) = (a.grid, b.grid);
    pending.lock().unwrap().push((0, a));
    pending.lock().unwrap().push((1, b));

    std::thread::sleep(Duration::from_millis(300)); // que el callback cargue los tracks

    engine::set_f32(&controls.gain[0], 1.0);
    engine::set_f32(&controls.gain[1], 1.0);
    engine::set_f32(&controls.master_gain, 0.9);
    engine::set_f32(&controls.crossfader, 0.5);
    controls.playing[0].store(true, Ordering::Relaxed);
    controls.playing[1].store(true, Ordering::Relaxed);

    let sr_f = sr as f64;
    let mut peak_max = 0.0f32;
    let mut silent_ticks = 0;
    let mut worst_err = 0.0f64;
    let frames_start = controls.frames_rendered.load(Ordering::Relaxed);
    let t0 = std::time::Instant::now();
    for _ in 0..(secs * 20) {
        std::thread::sleep(Duration::from_millis(50));
        let pos_a = controls.position[0].load(Ordering::Relaxed) as f64 / sr_f;
        let pos_b = controls.position[1].load(Ordering::Relaxed) as f64 / sr_f;
        let (rate, err) = sync::sync_relative_rate(pos_a, grid_a, pos_b, grid_b, 0.08);
        engine::set_f32(&controls.rate[1], rate as f32);
        worst_err = worst_err.max(err.abs());
        let p = engine::get_f32(&controls.master_peak);
        peak_max = peak_max.max(p);
        if p < 0.001 {
            silent_ticks += 1;
        }
    }

    let pos_a = controls.position[0].load(Ordering::Relaxed);
    let pos_b = controls.position[1].load(Ordering::Relaxed);
    let xruns = controls.xruns.load(Ordering::Relaxed);
    let wall = t0.elapsed().as_secs_f64();
    let rendered = controls
        .frames_rendered
        .load(Ordering::Relaxed)
        .saturating_sub(frames_start) as f64;
    let expecting = wall * sr_f;
    println!(
        "autoplay: {:.2} s de reloj | frames renderizados {} ({:.1} % de lo esperado) | pico master {:.3} | ticks en silencio {}/{} | xruns {}",
        wall,
        rendered as u64,
        rendered / expecting * 100.0,
        peak_max,
        silent_ticks,
        secs * 20,
        xruns
    );
    let block = controls.block_frames_max.load(Ordering::Relaxed);
    println!(
        "autoplay: bloque máximo del callback {} frames ({:.1} ms), último {}",
        block,
        block as f64 * 1000.0 / sr_f,
        controls.block_frames_last.load(Ordering::Relaxed)
    );
    println!(
        "autoplay: posiciones A={} B={} frames ({:.2} s de audio)",
        pos_a,
        pos_b,
        pos_b as f64 / sr_f
    );

    let mut ok = true;
    if pos_a == 0 || pos_b == 0 {
        println!("FALLO: los decks no avanzaron");
        ok = false;
    }
    if peak_max < 0.05 {
        println!("FALLO: no llegó señal al dispositivo");
        ok = false;
    }
    if ok {
        println!(
            "OK: dos decks sonando por el dispositivo, peor error de fase {:.3} beats",
            worst_err
        );
        Ok(())
    } else {
        std::process::exit(1)
    }
}

fn accent(deck: usize) -> (u8, u8, u8) {
    if deck == 0 {
        (34, 211, 238)
    } else {
        (251, 191, 36)
    }
}

fn toggle_play(controls: &Arc<Controls>, deck: usize) {
    let cur = controls.playing[deck].load(Ordering::Relaxed);
    controls.playing[deck].store(!cur, Ordering::Relaxed);
}

fn cue(controls: &Arc<Controls>, decks: &Rc<RefCell<[DeckState; DECK_COUNT]>>, deck: usize) {
    let st = decks.borrow();
    if let Some(track) = st[deck].track.as_ref() {
        let frame = (track.grid.first_beat * track.sample_rate as f64) as u64;
        controls.playing[deck].store(false, Ordering::Relaxed);
        controls.seek_request[deck].store(frame, Ordering::Relaxed);
    }
}

fn toggle_sync(decks: &Rc<RefCell<[DeckState; DECK_COUNT]>>, ui: &slint::Weak<AppWindow>, deck: usize) {
    let mut st = decks.borrow_mut();
    st[deck].syncing = !st[deck].syncing;
    let on = st[deck].syncing;
    drop(st);
    if let Some(ui) = ui.upgrade() {
        if deck == 0 {
            ui.set_synced_a(on);
        } else {
            ui.set_synced_b(on);
        }
    }
}

fn set_pitch(controls: &Arc<Controls>, decks: &Rc<RefCell<[DeckState; DECK_COUNT]>>, deck: usize, pct: f32) {
    {
        let mut st = decks.borrow_mut();
        st[deck].pitch_pct = pct as f64;
        if st[deck].syncing {
            return; // sync manda: el pitch manual queda ignorado mientras esté activo
        }
    }
    engine::set_f32(&controls.rate[deck], 1.0 + pct / 100.0);
}

fn fraction(pos: f64, len: f64) -> f64 {
    if len <= 0.0 {
        0.0
    } else {
        (pos / len).clamp(0.0, 1.0)
    }
}

fn fmt_time(pos: f64, len: f64) -> String {
    fn mmss(s: f64) -> String {
        let s = s.max(0.0);
        format!("{}:{:02}", (s / 60.0) as u64, (s % 60.0) as u64)
    }
    format!("{} / {}", mmss(pos), mmss(len))
}

/// Dibuja la waveform a partir de los picos: una barra vertical por columna.
fn wave_image(peaks: &Peaks, w: u32, h: u32, rgb: (u8, u8, u8)) -> Image {
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(w, h);
    {
        let pixels = buf.make_mut_slice();
        for p in pixels.iter_mut() {
            *p = Rgba8Pixel {
                r: 8,
                g: 11,
                b: 15,
                a: 255,
            };
        }
        let buckets = peaks.max.len().max(1);
        let mid = h as f32 / 2.0;
        // Normaliza al pico del track: un master muy comprimido no debe tocar
        // los bordes del panel como si estuviera recortado.
        let peak_amp = (0..buckets)
            .map(|b| peaks.max[b].abs().max(peaks.min[b].abs()))
            .fold(0.0f32, f32::max)
            .max(0.05);
        let norm = 0.92 / peak_amp;
        for x in 0..w {
            let b = (x as usize * buckets / w as usize).min(buckets - 1);
            let amp = (peaks.max[b].abs().max(peaks.min[b].abs()) * norm).clamp(0.02, 1.0);
            let half = (amp * mid).max(0.5);
            let y0 = (mid - half).max(0.0) as u32;
            let y1 = ((mid + half) as u32).min(h - 1);
            for y in y0..=y1 {
                let idx = (y * w + x) as usize;
                let dist = (y as f32 - mid).abs() / mid.max(1.0);
                let f = 1.0 - 0.45 * dist;
                pixels[idx] = Rgba8Pixel {
                    r: (rgb.0 as f32 * f) as u8,
                    g: (rgb.1 as f32 * f) as u8,
                    b: (rgb.2 as f32 * f) as u8,
                    a: 255,
                };
            }
        }
    }
    Image::from_rgba8(buf)
}

/// Track sintético: kick en cada beat, hat en la corchea, bajo y acorde.
/// Sirve para probar el motor y el sync sin depender de archivos.
fn make_demo_track(bpm: f64, sr: u32, detune_semitones: f64) -> Track {
    let seconds = 32.0;
    let frames = (seconds * sr as f64) as usize;
    let mut samples = vec![0.0f32; frames * 2];
    let beat = 60.0 / bpm;
    let root = 55.0 * 2f64.powf(detune_semitones / 12.0); // A1

    let mut t = 0.0;
    let mut beat_idx = 0usize;
    while t < seconds {
        let start = (t * sr as f64) as usize;

        // Kick: barrido 110 -> 45 Hz con caída rápida.
        let kick_len = (sr as f64 * 0.18) as usize;
        for i in 0..kick_len {
            let idx = start + i;
            if idx >= frames {
                break;
            }
            let tt = i as f64 / sr as f64;
            let f = 110.0 * (-tt * 18.0).exp() + 45.0;
            let env = (-tt * 12.0).exp();
            let v = ((tt * f * std::f64::consts::TAU).sin() * env * 0.55) as f32;
            samples[idx * 2] += v;
            samples[idx * 2 + 1] += v;
        }

        // Hat en la corchea: ruido corto.
        let hat_start = start + (beat * 0.5 * sr as f64) as usize;
        let hat_len = (sr as f64 * 0.04) as usize;
        let mut seed = 12345u32.wrapping_add(beat_idx as u32 * 7919);
        for i in 0..hat_len {
            let idx = hat_start + i;
            if idx >= frames {
                break;
            }
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let noise = ((seed >> 9) as f32 / (1 << 23) as f32) * 2.0 - 1.0;
            let env = 1.0 - i as f32 / hat_len as f32;
            let v = noise * env * 0.08;
            samples[idx * 2] += v;
            samples[idx * 2 + 1] += v;
        }

        // Bajo en cada beat: nota del patrón.
        let pattern = [0, 0, 7, 5];
        let semis = pattern[beat_idx % pattern.len()] as f64;
        let f = root * 2f64.powf(semis / 12.0);
        let bass_len = (beat * 0.9 * sr as f64) as usize;
        for i in 0..bass_len {
            let idx = start + i;
            if idx >= frames {
                break;
            }
            let tt = i as f64 / sr as f64;
            let env = (-tt * 4.0).exp() * (1.0 - tt / (beat * 0.9)).max(0.0);
            let v = ((tt * f * std::f64::consts::TAU).sin() * env * 0.22) as f32;
            samples[idx * 2] += v;
            samples[idx * 2 + 1] += v;
        }

        t += beat;
        beat_idx += 1;
    }

    Track::new(
        format!("demo {:.1} BPM", bpm),
        samples,
        sr,
        BeatGrid::new(bpm, 0.0),
    )
}
