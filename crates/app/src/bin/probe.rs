//! Bisección de la UI: reproduce un archivo por el MISMO camino de audio que la
//! app y va agregando partes de interfaz por etapas, para encontrar la que
//! ensucia el sonido.
//!
//! Uso: probe <archivo> <etapa 1..4>
//!   1: ventana + botón PLAY (sin timers)
//!   2: + timer de 30 Hz actualizando texto de posición
//!   3: + imagen (simulando la onda) redibujada a 30 Hz
//!   4: + medidor de nivel animado
//!
//! La base es la del reproductor sin ventana que ya se verificó limpia.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, StreamConfig};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SR: u32 = 48_000;
const BUFFER: u32 = 1024;

slint::slint! {
    import { Button } from "std-widgets.slint";

    export component Probe inherits Window {
        in-out property <string> info: "…";
        in-out property <string> pos: "0.0 s";
        in property <bool> playing: false;
        in-out property <image> onda;
        in property <float> nivel: 0.0;
        in property <int> etapa: 1;
        callback toggle();

        width: 720px;
        height: 340px;
        background: #0d1117;

        VerticalLayout {
            padding: 12px;
            spacing: 10px;

            Text {
                text: root.info;
                color: #dbe4ee;
                font-size: 13px;
                overflow: elide;
            }

            HorizontalLayout {
                spacing: 10px;
                Button {
                    text: root.playing ? "PAUSA" : "PLAY";
                    clicked => { root.toggle(); }
                }
                Text {
                    text: root.pos;
                    color: #8b98a8;
                    vertical-alignment: center;
                }
            }

            if root.etapa >= 3 : Image {
                source: root.onda;
                min-height: 110px;
            }

            if root.etapa >= 4 : Rectangle {
                height: 14px;
                background: #22d3ee;
                width: root.nivel * 400px;
            }

            Rectangle { vertical-stretch: 1; }
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = std::path::Path::new(&args[0]);
    let etapa: i32 = args.get(1).and_then(|v| v.parse().ok()).unwrap_or(1);
    eprintln!("etapa {etapa}: {}", match etapa {
        1 => "ventana + botón",
        2 => "+ timer 30 Hz con texto",
        3 => "+ imagen de onda a 30 Hz",
        _ => "+ medidor animado",
    });

    // Pedir la misma latencia que el quantum por defecto de PipeWire.
    if std::env::var_os("PIPEWIRE_LATENCY").is_none() {
        std::env::set_var("PIPEWIRE_LATENCY", format!("{BUFFER}/{SR}"));
    }

    let decoded = engine::decode_file(path).expect("decode");
    let samples = engine::resample_stereo(&decoded.samples, decoded.sample_rate, SR);
    let total = samples.len() as u64 / 2;
    eprintln!(
        "{}: {:.1} s | pico {:.3}",
        path.file_name().unwrap().to_string_lossy(),
        total as f64 / SR as f64,
        samples.iter().fold(0.0f32, |m, v| m.max(v.abs()))
    );

    let datos = Arc::new(Mutex::new(samples));
    let pos = Arc::new(AtomicU64::new(0));
    let sonando = Arc::new(AtomicBool::new(true));
    let frames = Arc::new(AtomicU64::new(0));
    let pico_cb = Arc::new(AtomicU64::new(0)); // f32 en bits

    let host = cpal::default_host();
    let device = host.default_output_device().expect("sin dispositivo");
    let config = StreamConfig {
        channels: 2,
        sample_rate: cpal::SampleRate(SR),
        buffer_size: BufferSize::Fixed(BUFFER),
    };
    let (d, p, s, f, pk) = (
        datos.clone(),
        pos.clone(),
        sonando.clone(),
        frames.clone(),
        pico_cb.clone(),
    );
    let stream = device
        .build_output_stream(
            &config,
            move |out: &mut [f32], _| {
                let buf = d.lock().unwrap();
                let mut i = p.load(Ordering::Relaxed);
                let mut maximo = 0.0f32;
                let activo = s.load(Ordering::Relaxed);
                for k in 0..out.len() / 2 {
                    if activo {
                        if i >= total {
                            i = 0;
                        }
                        let l = buf[(i as usize) * 2];
                        let r = buf[(i as usize) * 2 + 1];
                        out[k * 2] = l;
                        out[k * 2 + 1] = r;
                        maximo = maximo.max(l.abs()).max(r.abs());
                        i += 1;
                    } else {
                        out[k * 2] = 0.0;
                        out[k * 2 + 1] = 0.0;
                    }
                }
                p.store(i, Ordering::Relaxed);
                f.fetch_add(out.len() as u64 / 2, Ordering::Relaxed);
                pk.store(maximo.to_bits() as u64, Ordering::Relaxed);
            },
            |e| eprintln!("audio: {e}"),
            None,
        )
        .expect("no se pudo abrir el stream");
    stream.play().expect("play");

    let ui = Probe::new().expect("ventana");
    ui.set_etapa(etapa);
    ui.set_playing(true);
    ui.set_info(format!("{} — etapa {etapa}", path.file_name().unwrap().to_string_lossy()).into());
    {
        let s = sonando.clone();
        let ui_weak = ui.as_weak();
        ui.on_toggle(move || {
            let nuevo = !s.load(Ordering::Relaxed);
            s.store(nuevo, Ordering::Relaxed);
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_playing(nuevo);
            }
        });
    }

    // Etapa 2 y siguientes: timer de 30 Hz con texto de posición.
    let mut timer_guard = None;
    if etapa >= 2 {
        let pos = pos.clone();
        let pico_cb = pico_cb.clone();
        let sonando = sonando.clone();
        let ui_weak = ui.as_weak();
        let mut onda_estado: u32 = 0;
        let timer = slint::Timer::default();
        timer.start(slint::TimerMode::Repeated, Duration::from_millis(33), move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let t = pos.load(Ordering::Relaxed) as f64 / SR as f64;
            ui.set_pos(format!("{:.1} s", t).into());
            if etapa >= 3 {
                // Imagen que cambia en cada tick, como la onda real.
                onda_estado = onda_estado.wrapping_add(1);
                let (w, h) = (600u32, 110u32);
                let mut buf = slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(w, h);
                let phase = onda_estado as f32 * 0.15;
                {
                    let bytes = buf.make_mut_bytes();
                    for x in 0..w {
                        let v = ((x as f32 * 0.08 + phase).sin() * 0.5 + 0.5) * (h as f32 - 2.0);
                        for y in 0..h {
                            let dentro = (y as f32 - v).abs() < 12.0;
                            let (r, g, b) = if dentro { (34u8, 211u8, 238u8) } else { (7, 10, 14) };
                            let base = ((y * w + x) * 3) as usize;
                            bytes[base] = r;
                            bytes[base + 1] = g;
                            bytes[base + 2] = b;
                        }
                    }
                }
                ui.set_onda(slint::Image::from_rgb8(buf));
            }
            if etapa >= 4 {
                let pico = f32::from_bits(pico_cb.load(Ordering::Relaxed) as u32);
                ui.set_nivel((pico * 2.0).min(1.0));
            }
            let _ = sonando.load(Ordering::Relaxed);
        });
        timer_guard = Some(timer);
    }

    // Reporte a stderr: frames contra reloj de pared, la misma métrica del DIAG.
    let frames_cb = frames.clone();
    std::thread::spawn(move || {
        let t0 = Instant::now();
        let base = frames_cb.load(Ordering::Relaxed);
        loop {
            std::thread::sleep(Duration::from_secs(5));
            let el = t0.elapsed().as_secs_f64();
            let rend = frames_cb.load(Ordering::Relaxed) - base;
            eprintln!(
                "frames {:.2} % de lo esperado",
                100.0 * rend as f64 / (el * SR as f64).max(1.0)
            );
        }
    });

    ui.run().expect("event loop");
    drop(timer_guard);
    drop(stream);
}
