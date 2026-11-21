//! djdeck — MVP: dos decks, mixer, EQ y sync por beatgrid.
//!
//! Uso:
//!   djdeck "track-a.mp3" "track-b.flac"
//!   djdeck                              # demos sintéticos
//!   djdeck --selftest                   # verificación offline
//!   djdeck --autoplay=30                # verificación con la placa real

use analysis::PeakPyramid;
use anyhow::{Context, Result};
use engine::sync::{self, BeatGrid};
use engine::{Controls, PendingLoads, Track, DECK_COUNT};
use slint::{ComponentHandle, Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, Timer, TimerMode, VecModel};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 1024 frames ≈ 21 ms de buffer: medido estable en PipeWire (256 frames produce
/// underruns sostenidos y el reloj de audio se atrasa ~16 %). Se puede forzar con
/// `DJDECK_FRAMES`.
const DEFAULT_FRAMES: u32 = 1024;
/// Ancho del bitmap de cada onda; Slint lo escala al ancho real.
const LANE_W: u32 = 1180;
/// División de resolución del buffer de ondas. El `Image` se estira a lo ancho
/// del carril, así que dibujar a la mitad de píxeles cuesta un cuarto y se ve
/// prácticamente igual. Importa porque el render de ondas es todo el costo del
/// tick de UI (medido: 3-7 ms), y ese tick le roba el turno al hilo de audio en
/// el mismo proceso (se escucha como chispeo).
/* WAVE_DIV: 2 (mitad de píxeles) no cambió el costo del tick: lo que cuesta es el repintado del Image, no el dibujo. Se deja en 2 por calidad visual. */
const WAVE_DIV: u32 = 2;
/// Cuántos carriles de ancho mide la tira de onda que se dibuja de una vez.
const STRIP: u32 = 3;
const LANE_H: u32 = 108;
/// Piso del alto del bitmap: por debajo de esto la onda no tiene detalle útil.
const LANE_MIN_PY: i64 = 40;

slint::slint! {
    import { Button, ScrollView, Slider } from "std-widgets.slint";

    component VuMeter inherits Rectangle {
        in property <float> level: 0.0;
        in property <color> accent: #22d3ee;
        width: 14px;
        background: #070a0e;
        border-radius: 3px;
        Rectangle {
            y: parent.height - parent.height * Math.min(1.0, root.level);
            width: parent.width;
            height: parent.height * Math.min(1.0, root.level);
            background: root.accent;
            border-radius: 3px;
        }
    }

    export struct BrowserRow {
        name: string,
        is-dir: bool,
    }

    component Browser inherits Rectangle {
        in property <string> path-text: "";
        in property <[BrowserRow]> rows;
        callback pick(int);
        callback up();
        callback close();

        background: #000000b4;

        panel := Rectangle {
            width: 72%;
            height: 74%;
            background: #0f151c;
            border-radius: 10px;
            border-width: 1px;
            border-color: #2a3746;

            VerticalLayout {
                padding: 12px;
                spacing: 8px;

                HorizontalLayout {
                    spacing: 8px;
                    Text { text: "CARGAR EN DECK"; color: #66727f; font-size: 11px; font-weight: 700; vertical-alignment: center; }
                    Text { text: root.path-text; color: #93a1b1; font-size: 11px; overflow: elide; horizontal-stretch: 1; vertical-alignment: center; }
                    Button { text: "SUPERIOR"; clicked => { root.up(); } }
                    Button { text: "CERRAR"; clicked => { root.close(); } }
                }

                ScrollView {
                    vertical-stretch: 1;
                    VerticalLayout {
                        spacing: 1px;
                        for row[i] in root.rows: Rectangle {
                            height: 26px;
                            background: ta.has-hover ? #1a2531 : transparent;
                            border-radius: 4px;
                            ta := TouchArea {
                                clicked => { root.pick(i); }
                            }
                            HorizontalLayout {
                                padding-left: 8px;
                                spacing: 8px;
                                Text {
                                    text: row.is-dir ? "▸" : "♪";
                                    color: row.is-dir ? #22d3ee : #7e8b9a;
                                    font-size: 11px;
                                    width: 14px;
                                    vertical-alignment: center;
                                }
                                Text {
                                    text: row.name;
                                    color: row.is-dir ? #e3ecf5 : #a9b6c4;
                                    font-size: 12px;
                                    overflow: elide;
                                    vertical-alignment: center;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Micro botón del zoom que vive entre las dos ondas. Va hecho a mano con
    // Rectangle + Text + TouchArea y no con `Button` de std-widgets: ese se
    // estira en vertical dentro de un layout y el pedido es un control chico
    // (18x16 px) que no crezca al maximizar.
    component ZoomButton inherits Rectangle {
        in property <string> text: "+";
        callback clicked();

        width: 18px;
        height: 16px;
        border-radius: 4px;
        border-width: 1px;
        border-color: #2b3644;
        background: touch.has-hover ? #26313d : #1a222c;

        Text {
            text: root.text;
            color: touch.pressed ? #eef4fa : #b6c2cf;
            font-size: 12px;
            font-weight: 700;
            x: 0px;
            y: 0px;
            width: parent.width;
            height: parent.height;
            horizontal-alignment: center;
            vertical-alignment: center;
        }

        touch := TouchArea {
            clicked => { root.clicked(); }
        }
    }

    component Lane inherits Rectangle {
        in property <string> label: "A";
        in property <string> track-name: "sin cargar";
        in property <string> bpm-text: "—";
        in property <string> time-text: "0:00";
        in property <image> img;
        // Desplazamiento de la tira de onda: la imagen se dibuja 3 carriles de
        // ancho y se corre, en vez de redibujarla 30 veces por segundo.
        in property <length> img-x: 0px;
        // Sombra del loop activo, en píxeles del carril. Se calcula en el
        // controlador a partir de la posición actual: el playhead está en el
        // centro, así que la sombra se corre junto con la onda.
        in property <bool> loop-visible: false;
        in property <length> loop-x: 0px;
        in property <length> loop-w: 0px;
        in property <color> accent: #22d3ee;
        in property <bool> synced: false;
        // Tamaño real del carril en pantalla, medido por el layout. El
        // controlador lo necesita para mapear frames a píxeles: al maximizar la
        // ventana el carril cambia de ancho y las constantes de diseño ya no
        // sirven.
        out property <length> real-w: body.width;
        out property <length> real-h: body.height;

        VerticalLayout {
            spacing: 3px;
            HorizontalLayout {
                spacing: 8px;
                height: 16px;
                Text { text: root.label; color: root.accent; font-size: 12px; font-weight: 800; vertical-alignment: center; }
                Text { text: root.track-name; color: #a9b6c4; font-size: 11px; overflow: elide; vertical-alignment: center; }
                Rectangle { horizontal-stretch: 1; }
                Text { text: root.synced ? "SYNC" : ""; color: root.accent; font-size: 10px; font-weight: 700; vertical-alignment: center; }
                Text { text: root.bpm-text; color: #8b98a8; font-size: 11px; vertical-alignment: center; }
                Text { text: root.time-text; color: #66727f; font-size: 11px; vertical-alignment: center; }
            }
            body := Rectangle {
                // El carril crece con la ventana, pero nunca baja de 90 px:
                // es el alto con el que la onda se lee bien en la ventana
                // inicial. El bitmap se redibuja a la altura real (real-h).
                min-height: 90px;
                vertical-stretch: 1;
                background: #070a0e;
                border-radius: 6px;
                clip: true;
                Image { source: root.img; x: root.img-x; width: 300%; height: 100%; }

                // Sombra del loop: un velo tenue del color del deck con dos
                // filos de 1 px, para ver exactamente qué tramo abarca sin
                // tapar la onda.
                Rectangle {
                    visible: root.loop-visible && root.loop-w > 1px;
                    x: root.loop-x;
                    width: root.loop-w;
                    height: 100%;
                    background: root.accent.with-alpha(0.08);

                    Rectangle {
                        x: 0px;
                        width: 1px;
                        height: 100%;
                        background: root.accent.with-alpha(0.35);
                    }
                    Rectangle {
                        x: root.loop-w - 1px;
                        width: 1px;
                        height: 100%;
                        background: root.accent.with-alpha(0.35);
                    }
                }

                // Playhead: referencia vertical fija al centro del carril. Va
                // dibujado acá y no dentro del bitmap porque la tira se corre
                // con el scroll: adentro de la imagen la línea se iba con la
                // onda y saltaba al centro en cada redibujado.
                Rectangle {
                    x: (parent.width - 2px) / 2;
                    width: 2px;
                    height: 100%;
                    background: #f0f5fa;
                }
            }
        }
    }

    // Fader horizontal con marca del valor por defecto y doble click para
    // volver a él. Lo usa el pitch: doble click en el fader = 0.0 % (centro).
    // No asigna `value` por dentro: sólo emite `changed`, así el rate del motor
    // sigue siendo la única fuente de verdad y el lazo de sync manda el fader.
    component Fader inherits Rectangle {
        in property <float> minimum: -8.0;
        in property <float> maximum: 8.0;
        in property <float> reset-value: 0.0;
        in property <color> accent: #0f6b7a;
        in-out property <float> value: 0.0;
        callback changed(float);
        callback reset();

        height: 26px;
        horizontal-stretch: 1;
        property <length> handle-w: 10px;
        property <float> span: Math.max(root.maximum - root.minimum, 0.000001);
        property <float> frac: Math.min(1.0, Math.max(0.0, (root.value - root.minimum) / root.span));
        property <float> reset-frac: Math.min(1.0, Math.max(0.0, (root.reset-value - root.minimum) / root.span));
        property <length> track-x: root.handle-w / 2;
        property <length> track-w: Math.max(root.width - root.handle-w, 1px);
        property <length> handle-x: root.track-x + root.track-w * root.frac - root.handle-w / 2;
        property <length> reset-x: root.track-x + root.track-w * root.reset-frac;
        property <length> fill-x: root.track-x + root.track-w * Math.min(root.frac, root.reset-frac);
        property <length> fill-w: root.track-w * Math.abs(root.frac - root.reset-frac);

        function frac-at(x: length) -> float {
            Math.min(1.0, Math.max(0.0, (x - root.track-x) / root.track-w))
        }

        // carril
        Rectangle {
            x: root.track-x;
            width: root.track-w;
            y: (root.height - self.height) / 2;
            height: 4px;
            background: #1f2833;
            border-radius: 2px;
        }

        // recorrido desde el centro hasta la perilla
        Rectangle {
            x: root.fill-x;
            width: root.fill-w;
            y: (root.height - self.height) / 2;
            height: 4px;
            background: root.accent.with-alpha(0.45);
            border-radius: 2px;
        }

        // marca del valor por defecto (en el pitch: el centro)
        Rectangle {
            x: root.reset-x - 0.5px;
            width: 1px;
            y: (root.height - 14px) / 2;
            height: 14px;
            background: #66727f;
        }

        // perilla
        Rectangle {
            x: root.handle-x;
            width: root.handle-w;
            y: 2px;
            height: root.height - 4px;
            background: touch.pressed ? #e8eef5 : #93a0af;
            border-width: 1px;
            border-color: #2b3644;
            border-radius: 3px;
        }

        touch := TouchArea {
            moved => { root.changed(root.minimum + root.frac-at(self.mouse-x) * root.span); }
            clicked => { root.changed(root.minimum + root.frac-at(self.mouse-x) * root.span); }
            double-clicked => { root.reset(); }
        }
    }

    // Iconos: paths de Material Symbols (Apache-2.0, Google LLC), variante
    // rounded rellena. Origen y licencia en crates/app/icons/NOTICE.md.
    // Van como `Path` con viewbox 24x24 y no como glifos de texto porque el
    // glifo ⏸ (U+23F8) no está en DejaVu Sans en este equipo —sólo en
    // Symbola/FreeSerif—, y un glifo faltante se ve como un cuadrado vacío.
    // Todos los elegidos son formas rellenas sin hueco: el renderer software no
    // respeta el agujero de un path con subpaths de winding opuesto (probado con
    // trazo, con arcos y con polígono; en los tres casos rellena el centro).
    enum DeckIcon { play, pause, cue, sync, folder, repeat }

    component DeckGlyph inherits Rectangle {
        in property <DeckIcon> kind: DeckIcon.play;
        in property <color> ink: #b6c2cf;
        background: transparent;

        if kind == DeckIcon.play : Path {
            width: 100%;
            height: 100%;
            viewbox-width: 24;
            viewbox-height: 24;
            commands: "M8 6.82v10.36c0 .79.87 1.27 1.54.84l8.14-5.18c.62-.39.62-1.29 0-1.69L9.54 5.98C8.87 5.55 8 6.03 8 6.82z";
            fill: root.ink;
        }

        if kind == DeckIcon.pause : Path {
            width: 100%;
            height: 100%;
            viewbox-width: 24;
            viewbox-height: 24;
            commands: "M8 19c1.1 0 2-.9 2-2V7c0-1.1-.9-2-2-2s-2 .9-2 2v10c0 1.1.9 2 2 2zm6-12v10c0 1.1.9 2 2 2s2-.9 2-2V7c0-1.1-.9-2-2-2s-2 .9-2 2z";
            fill: root.ink;
        }

        // CUE: barra + triángulo hacia el punto marcado (skip_previous)
        if kind == DeckIcon.cue : Path {
            width: 100%;
            height: 100%;
            viewbox-width: 24;
            viewbox-height: 24;
            commands: "M7 6c.55 0 1 .45 1 1v10c0 .55-.45 1-1 1s-1-.45-1-1V7c0-.55.45-1 1-1zm3.66 6.82l5.77 4.07c.66.47 1.58-.01 1.58-.82V7.93c0-.81-.91-1.28-1.58-.82l-5.77 4.07c-.57.4-.57 1.24 0 1.64z";
            fill: root.ink;
        }

        // SYNC: dos flechas en círculo
        if kind == DeckIcon.sync : Path {
            width: 100%;
            height: 100%;
            viewbox-width: 24;
            viewbox-height: 24;
            commands: "M12 4V2.21c0-.45-.54-.67-.85-.35l-2.8 2.79c-.2.2-.2.51 0 .71l2.79 2.79c.32.31.86.09.86-.36V6c3.31 0 6 2.69 6 6 0 .79-.15 1.56-.44 2.25-.15.36-.04.77.23 1.04.51.51 1.37.33 1.64-.34.37-.91.57-1.91.57-2.95 0-4.42-3.58-8-8-8zm0 14c-3.31 0-6-2.69-6-6 0-.79.15-1.56.44-2.25.15-.36.04-.77-.23-1.04-.51-.51-1.37-.33-1.64.34C4.2 9.96 4 10.96 4 12c0 4.42 3.58 8 8 8v1.79c0 .45.54.67.85.35l2.79-2.79c.2-.2.2-.51 0-.71l-2.79-2.79c-.31-.31-.85-.09-.85.36V18z";
            fill: root.ink;
        }

        // CARGAR: carpeta rellena (la variante folder_open tiene hueco y el
        // renderer software la dibuja maciza, así que se usa `folder`)
        if kind == DeckIcon.folder : Path {
            width: 100%;
            height: 100%;
            viewbox-width: 24;
            viewbox-height: 24;
            commands: "M10.59 4.59C10.21 4.21 9.7 4 9.17 4H4c-1.1 0-1.99.9-1.99 2L2 18c0 1.1.9 2 2 2h16c1.1 0 2-.9 2-2V8c0-1.1-.9-2-2-2h-8l-1.41-1.41z";
            fill: root.ink;
        }

        // LOOP: flechas de repetición; el largo en beats va al lado como número
        if kind == DeckIcon.repeat : Path {
            width: 100%;
            height: 100%;
            viewbox-width: 24;
            viewbox-height: 24;
            commands: "M7 7h10v1.79c0 .45.54.67.85.35l2.79-2.79c.2-.2.2-.51 0-.71l-2.79-2.79c-.31-.31-.85-.09-.85.36V5H6c-.55 0-1 .45-1 1v4c0 .55.45 1 1 1s1-.45 1-1V7zm10 10H7v-1.79c0-.45-.54-.67-.85-.35l-2.79 2.79c-.2.2-.2.51 0 .71l2.79 2.79c.31.31.85.09.85-.36V19h11c.55 0 1-.45 1-1v-4c0-.55-.45-1-1-1s-1 .45-1 1v3z";
            fill: root.ink;
        }
    }

    // Botón del transporte: icono (y número opcional para el loop) en vez de
    // texto. Las posiciones son explícitas: dentro de un HorizontalLayout el
    // glifo de altura fija queda pegado arriba, 5 px fuera del centro.
    component DeckButton inherits Rectangle {
        in property <DeckIcon> kind: DeckIcon.play;
        in property <string> number: "";
        in property <bool> show-icon: true;
        in property <bool> active: false;
        in property <color> accent: #22d3ee;
        callback clicked();

        width: 34px;
        height: 26px;
        border-radius: 5px;
        border-width: 1px;
        border-color: root.active ? root.accent.with-alpha(0.8) : #2b3644;
        background: root.active ? root.accent.with-alpha(0.22) : (touch.has-hover ? #26313d : #1a222c);

        property <color> ink: root.active ? root.accent : (touch.pressed ? #eef4fa : #b6c2cf);
        property <length> glyph: 18px;

        DeckGlyph {
            visible: root.show-icon;
            kind: root.kind;
            ink: root.ink;
            width: root.glyph;
            height: root.glyph;
            x: root.number == "" ? (root.width - root.glyph) / 2 : 3px;
            y: (root.height - root.glyph) / 2;
        }

        Text {
            visible: root.number != "";
            text: root.number;
            color: root.ink;
            font-size: 13px;
            font-weight: 700;
            x: 0px;
            width: root.show-icon ? root.width - 7px : root.width;
            height: root.height;
            horizontal-alignment: root.show-icon ? right : center;
            vertical-alignment: center;
        }

        touch := TouchArea {
            clicked => { root.clicked(); }
        }
    }

    component DeckPanel inherits Rectangle {
        in property <string> label: "A";
        in property <string> track-name: "sin cargar";
        in property <string> bpm-text: "—";
        in property <string> pitch-text: "+0.0%";
        in property <string> gain-text: "80%";
        in property <string> low-text: "0.0 dB";
        in property <string> mid-text: "0.0 dB";
        in property <string> high-text: "0.0 dB";
        in property <bool> playing: false;
        in property <bool> synced: false;
        in property <bool> looping: false;
        // Largo del loop activo en beats: 0 sin loop, 4 u 8 con el botón encendido.
        in property <int> loop-beats: 0;
        in property <float> meter: 0.0;
        in-out property <float> pitch-pct: 0.0;
        in-out property <float> gain: 0.8;
        in-out property <float> eq-low: 0.0;
        in-out property <float> eq-mid: 0.0;
        in-out property <float> eq-high: 0.0;
        in property <color> accent: #22d3ee;

        callback play();
        callback cue();
        callback sync();
        callback browse();
        callback loop8();
        callback loop4();
        callback pitch-changed(float);
        callback pitch-reset();
        callback gain-changed(float);
        callback eq-low-changed(float);
        callback eq-mid-changed(float);
        callback eq-high-changed(float);

        background: #12171d;
        border-radius: 10px;
        border-width: 1px;
        border-color: #1f2833;

        VerticalLayout {
            padding: 12px;
            spacing: 8px;
            // space-between: la primera fila queda arriba y la última (HIGH)
            // abajo; los seis huecos entre las siete filas quedan iguales.
            // Con `alignment: center` el bloque quedaba centrado y al
            // maximizar sobraban ~550 px muertos arriba y abajo.
            alignment: space-between;

            HorizontalLayout {
                spacing: 8px;
                Text { text: root.label; color: root.accent; font-size: 18px; font-weight: 800; }
                Text { text: root.track-name; color: #dbe4ee; font-size: 12px; overflow: elide; vertical-alignment: center; }
                Rectangle { horizontal-stretch: 1; }
                Text { text: root.bpm-text; color: #8b98a8; font-size: 12px; vertical-alignment: center; }
            }

            HorizontalLayout {
                spacing: 6px;
                DeckButton {
                    kind: root.playing ? DeckIcon.pause : DeckIcon.play;
                    accent: root.accent;
                    clicked => { root.play(); }
                }
                DeckButton { kind: DeckIcon.cue; accent: root.accent; clicked => { root.cue(); } }
                DeckButton { kind: DeckIcon.sync; active: root.synced; accent: root.accent; clicked => { root.sync(); } }
                DeckButton { kind: DeckIcon.folder; accent: root.accent; clicked => { root.browse(); } }
                DeckButton {
                    kind: DeckIcon.repeat;
                    number: "8";
                    active: root.looping && root.loop-beats == 8;
                    accent: root.accent;
                    width: 42px;
                    clicked => { root.loop8(); }
                }
                DeckButton {
                    kind: DeckIcon.repeat;
                    number: "4";
                    active: root.looping && root.loop-beats == 4;
                    accent: root.accent;
                    width: 42px;
                    clicked => { root.loop4(); }
                }
                Rectangle { horizontal-stretch: 1; }
                VuMeter { level: root.meter; accent: root.accent; width: 10px; height: 26px; }
            }

            HorizontalLayout {
                spacing: 6px;
                Text { text: "PITCH"; color: #66727f; font-size: 10px; width: 40px; vertical-alignment: center; }
                Fader {
                    minimum: -8.0;
                    maximum: 8.0;
                    reset-value: 0.0;
                    accent: root.accent;
                    value: root.pitch-pct;
                    changed(v) => { root.pitch-changed(v); }
                    reset => { root.pitch-reset(); }
                }
                Text { text: root.pitch-text; color: #dbe4ee; font-size: 11px; width: 22%; min-width: 58px; horizontal-alignment: right; vertical-alignment: center; }
            }

            HorizontalLayout {
                spacing: 6px;
                Text { text: "GAIN"; color: #66727f; font-size: 10px; width: 40px; vertical-alignment: center; }
                Slider {
                    minimum: 0.0; maximum: 1.5; value: root.gain;
                    changed(v) => { root.gain = v; root.gain-changed(v); }
                }
                Text { text: root.gain-text; color: #dbe4ee; font-size: 11px; width: 22%; min-width: 58px; horizontal-alignment: right; vertical-alignment: center; }
            }

            HorizontalLayout {
                spacing: 6px;
                Text { text: "LOW"; color: #66727f; font-size: 10px; width: 40px; vertical-alignment: center; }
                Slider {
                    minimum: -12.0; maximum: 12.0; value: root.eq-low;
                    changed(v) => { root.eq-low = v; root.eq-low-changed(v); }
                }
                Text { text: root.low-text; color: #dbe4ee; font-size: 11px; width: 22%; min-width: 58px; horizontal-alignment: right; vertical-alignment: center; }
            }

            HorizontalLayout {
                spacing: 6px;
                Text { text: "MID"; color: #66727f; font-size: 10px; width: 40px; vertical-alignment: center; }
                Slider {
                    minimum: -12.0; maximum: 12.0; value: root.eq-mid;
                    changed(v) => { root.eq-mid = v; root.eq-mid-changed(v); }
                }
                Text { text: root.mid-text; color: #dbe4ee; font-size: 11px; width: 22%; min-width: 58px; horizontal-alignment: right; vertical-alignment: center; }
            }

            HorizontalLayout {
                spacing: 6px;
                Text { text: "HIGH"; color: #66727f; font-size: 10px; width: 40px; vertical-alignment: center; }
                Slider {
                    minimum: -12.0; maximum: 12.0; value: root.eq-high;
                    changed(v) => { root.eq-high = v; root.eq-high-changed(v); }
                }
                Text { text: root.high-text; color: #dbe4ee; font-size: 11px; width: 22%; min-width: 58px; horizontal-alignment: right; vertical-alignment: center; }
            }
        }
    }

    component MixerStrip inherits Rectangle {
        in property <float> meter-a: 0.0;
        in property <float> meter-b: 0.0;
        in property <float> master: 0.0;
        in-out property <float> xf: 0.5;
        callback xf-changed(float);

        background: #0f1419;
        border-radius: 10px;
        border-width: 1px;
        border-color: #1f2833;

        VerticalLayout {
            padding: 12px;
            spacing: 10px;
            // stretch + separadores con vertical-stretch: el alto sobrante se
            // reparte en huecos iguales (medido: 71 px a 1080p, 107 px a 1400)
            // y los vumetros crecen con tope, sin quedar como hilos.
            alignment: stretch;

            Text { text: "MIXER"; color: #66727f; font-size: 10px; font-weight: 700; horizontal-alignment: center; }

            Rectangle { vertical-stretch: 1; }

            HorizontalLayout {
                spacing: 14px;
                alignment: center;
                // 160 px es el alto mínimo legible; el tope de 380 evita que en
                // una ventana muy alta los medidores queden como tiras.
                min-height: 160px;
                max-height: 380px;
                vertical-stretch: 2;
                VuMeter { level: root.meter-a; accent: #22d3ee; }
                VuMeter { level: root.meter-b; accent: #fbbf24; }
                VuMeter { level: root.master; accent: #9aa7b6; }
            }

            Rectangle { vertical-stretch: 1; }

            Text { text: "A · B · MASTER"; color: #4b5663; font-size: 9px; horizontal-alignment: center; }

            Rectangle { vertical-stretch: 1; }

            HorizontalLayout {
                spacing: 6px;
                Text { text: "A"; color: #22d3ee; font-size: 12px; font-weight: 800; vertical-alignment: center; }
                Slider {
                    minimum: 0.0; maximum: 1.0; value: root.xf;
                    changed(v) => { root.xf = v; root.xf-changed(v); }
                }
                Text { text: "B"; color: #fbbf24; font-size: 12px; font-weight: 800; vertical-alignment: center; }
            }

            Rectangle { vertical-stretch: 1; }

            Text { text: "CROSSFADER"; color: #4b5663; font-size: 9px; horizontal-alignment: center; }
        }
    }

    export component AppWindow inherits Window {
        title: "djdeck";
        // Sin width/height acá: si se declaran, el contenido queda clavado en ese
        // tamaño y al maximizar la ventana el layout no se re-acomoda (quedaba
        // una banda muerta a la derecha). El tamaño inicial se fija desde Rust
        // con `set_size`, que es el mismo camino que usa el usuario al
        // redimensionar.
        background: #0a0e13;

        in-out property <string> name-a: "sin cargar";
        in-out property <string> name-b: "sin cargar";
        in-out property <string> bpm-a: "—";
        in-out property <string> bpm-b: "—";
        in-out property <string> time-a: "0:00";
        in-out property <string> time-b: "0:00";
        in-out property <string> pitch-a-text: "+0.0%";
        in-out property <string> pitch-b-text: "+0.0%";
        in-out property <string> gain-a-text: "80%";
        in-out property <string> gain-b-text: "80%";
        in-out property <string> low-a-text: "0.0 dB";
        in-out property <string> low-b-text: "0.0 dB";
        in-out property <string> mid-a-text: "0.0 dB";
        in-out property <string> mid-b-text: "0.0 dB";
        in-out property <string> high-a-text: "0.0 dB";
        in-out property <string> high-b-text: "0.0 dB";
        in-out property <bool> playing-a: false;
        in-out property <bool> playing-b: false;
        in-out property <bool> synced-a: false;
        in-out property <bool> synced-b: false;
        in-out property <bool> looping-a: false;
        in-out property <bool> looping-b: false;
        in-out property <int> loop-beats-a: 0;
        in-out property <int> loop-beats-b: 0;
        in-out property <float> meter-a: 0.0;
        in-out property <float> meter-b: 0.0;
        in-out property <float> meter-master: 0.0;
        in-out property <float> pitch-a: 0.0;
        in-out property <float> pitch-b: 0.0;
        in-out property <float> gain-a: 0.8;
        in-out property <float> gain-b: 0.8;
        in-out property <float> eq-low-a: 0.0;
        in-out property <float> eq-low-b: 0.0;
        in-out property <float> eq-mid-a: 0.0;
        in-out property <float> eq-mid-b: 0.0;
        in-out property <float> eq-high-a: 0.0;
        in-out property <float> eq-high-b: 0.0;
        in-out property <float> xf: 0.5;
        in-out property <image> wave-a;
        in-out property <length> wave-a-x: 0px;
        in-out property <bool> loop-a-visible: false;
        in-out property <length> loop-a-x: 0px;
        in-out property <length> loop-a-w: 0px;
        in-out property <image> wave-b;
        in-out property <length> wave-b-x: 0px;
        in-out property <bool> loop-b-visible: false;
        in-out property <length> loop-b-x: 0px;
        in-out property <length> loop-b-w: 0px;
        // Tamaño real de cada carril, medido por el layout. Es la única fuente
        // de verdad para mapear frames a píxeles cuando la ventana cambia.
        out property <length> lane-a-w: lane-a.real-w;
        out property <length> lane-a-h: lane-a.real-h;
        out property <length> lane-b-w: lane-b.real-w;
        out property <length> lane-b-h: lane-b.real-h;
        in-out property <string> zoom-text: "8 s";
        in-out property <string> status: "";
        in-out property <bool> browser-open: false;
        in-out property <string> browser-path: "";
        in-out property <[BrowserRow]> browser-rows;

        callback play-a();
        callback play-b();
        callback cue-a();
        callback cue-b();
        callback sync-a();
        callback sync-b();
        callback pitch-a-changed(float);
        callback pitch-b-changed(float);
        callback pitch-reset-a();
        callback pitch-reset-b();
        callback gain-a-changed(float);
        callback gain-b-changed(float);
        callback eq-low-a-changed(float);
        callback eq-low-b-changed(float);
        callback eq-mid-a-changed(float);
        callback eq-mid-b-changed(float);
        callback eq-high-a-changed(float);
        callback eq-high-b-changed(float);
        callback xf-changed(float);
        callback zoom-in();
        callback zoom-out();
        callback browse-a();
        callback browse-b();
        callback loop-a(int);
        callback loop-b(int);
        callback browser-pick(int);
        callback browser-up();
        callback browser-close();

        VerticalLayout {
            padding: 12px;
            spacing: 10px;

            // El panel de ondas se lleva 1/3 del alto sobrante y la fila de
            // paneles 2/3: así al maximizar también crecen las ondas, pero
            // menos que los controles, que necesitan el alto para repartirse.
            Rectangle {
                vertical-stretch: 1;
                background: #0c1116;
                border-radius: 10px;
                border-width: 1px;
                border-color: #1a222c;

                VerticalLayout {
                    padding: 10px;
                    spacing: 6px;

                    // Los carriles se reparten el alto del panel: en la
                    // ventana inicial miden 109 px (90 de onda) y al maximizar
                    // crecen. La fila de zoom queda fija entre los dos.
                    lane-a := Lane {
                        vertical-stretch: 1;
                        label: "A";
                        accent: #22d3ee;
                        track-name: root.name-a;
                        bpm-text: root.bpm-a;
                        time-text: root.time-a;
                        synced: root.synced-a;
                        img: root.wave-a;
                        img-x: root.wave-a-x;
                        loop-visible: root.loop-a-visible;
                        loop-x: root.loop-a-x;
                        loop-w: root.loop-a-w;
                    }

                    // Zoom compacto entre las dos ondas. Alto fijo de 18 px y
                    // posiciones explícitas: dentro de un layout los controles
                    // de alto fijo quedan pegados arriba, y acá no puede
                    // quedar ningún rectángulo que se lleve el alto sobrante.
                    // Va centrado horizontalmente: a mitad de camino entre los
                    // dos carriles se lee como control común a los dos.
                    Rectangle {
                        height: 18px;
                        background: transparent;

                        // − (18) + 4 + texto (44) + 4 + + (18) = 88 px
                        Text {
                            text: root.zoom-text;
                            color: #8b98a8;
                            font-size: 11px;
                            x: (parent.width - 88px) / 2 + 22px;
                            y: 0px;
                            width: 44px;
                            height: 18px;
                            horizontal-alignment: center;
                            vertical-alignment: center;
                        }
                        ZoomButton {
                            text: "−";
                            x: (parent.width - 88px) / 2;
                            y: 1px;
                            clicked => { root.zoom-out(); }
                        }
                        ZoomButton {
                            text: "+";
                            x: (parent.width - 88px) / 2 + 70px;
                            y: 1px;
                            clicked => { root.zoom-in(); }
                        }
                    }

                    lane-b := Lane {
                        vertical-stretch: 1;
                        label: "B";
                        accent: #fbbf24;
                        track-name: root.name-b;
                        bpm-text: root.bpm-b;
                        time-text: root.time-b;
                        synced: root.synced-b;
                        img: root.wave-b;
                        img-x: root.wave-b-x;
                        loop-visible: root.loop-b-visible;
                        loop-x: root.loop-b-x;
                        loop-w: root.loop-b-w;
                    }
                }
            }

            HorizontalLayout {
                spacing: 10px;
                vertical-stretch: 2;

                DeckPanel {
                    width: 35%;
                    label: "A";
                    accent: #22d3ee;
                    track-name: root.name-a;
                    bpm-text: root.bpm-a;
                    pitch-text: root.pitch-a-text;
                    gain-text: root.gain-a-text;
                    low-text: root.low-a-text;
                    mid-text: root.mid-a-text;
                    high-text: root.high-a-text;
                    playing: root.playing-a;
                    synced: root.synced-a;
                    looping: root.looping-a;
                    loop-beats: root.loop-beats-a;
                    meter: root.meter-a;
                    pitch-pct: root.pitch-a;
                    gain: root.gain-a;
                    eq-low: root.eq-low-a;
                    eq-mid: root.eq-mid-a;
                    eq-high: root.eq-high-a;
                    play => { root.play-a(); }
                    cue => { root.cue-a(); }
                    sync => { root.sync-a(); }
                    browse => { root.browse-a(); }
                    loop8 => { root.loop-a(8); }
                    loop4 => { root.loop-a(4); }
                    pitch-changed(v) => { root.pitch-a = v; root.pitch-a-changed(v); }
                    pitch-reset => { root.pitch-reset-a(); }
                    gain-changed(v) => { root.gain-a = v; root.gain-a-changed(v); }
                    eq-low-changed(v) => { root.eq-low-a = v; root.eq-low-a-changed(v); }
                    eq-mid-changed(v) => { root.eq-mid-a = v; root.eq-mid-a-changed(v); }
                    eq-high-changed(v) => { root.eq-high-a = v; root.eq-high-a-changed(v); }
                }

                MixerStrip {
                    width: 26%;
                    meter-a: root.meter-a;
                    meter-b: root.meter-b;
                    master: root.meter-master;
                    xf: root.xf;
                    xf-changed(v) => { root.xf = v; root.xf-changed(v); }
                }

                DeckPanel {
                    width: 35%;
                    label: "B";
                    accent: #fbbf24;
                    track-name: root.name-b;
                    bpm-text: root.bpm-b;
                    pitch-text: root.pitch-b-text;
                    gain-text: root.gain-b-text;
                    low-text: root.low-b-text;
                    mid-text: root.mid-b-text;
                    high-text: root.high-b-text;
                    playing: root.playing-b;
                    synced: root.synced-b;
                    looping: root.looping-b;
                    loop-beats: root.loop-beats-b;
                    meter: root.meter-b;
                    pitch-pct: root.pitch-b;
                    gain: root.gain-b;
                    eq-low: root.eq-low-b;
                    eq-mid: root.eq-mid-b;
                    eq-high: root.eq-high-b;
                    play => { root.play-b(); }
                    cue => { root.cue-b(); }
                    sync => { root.sync-b(); }
                    browse => { root.browse-b(); }
                    loop8 => { root.loop-b(8); }
                    loop4 => { root.loop-b(4); }
                    pitch-changed(v) => { root.pitch-b = v; root.pitch-b-changed(v); }
                    pitch-reset => { root.pitch-reset-b(); }
                    gain-changed(v) => { root.gain-b = v; root.gain-b-changed(v); }
                    eq-low-changed(v) => { root.eq-low-b = v; root.eq-low-b-changed(v); }
                    eq-mid-changed(v) => { root.eq-mid-b = v; root.eq-mid-b-changed(v); }
                    eq-high-changed(v) => { root.eq-high-b = v; root.eq-high-b-changed(v); }
                }
            }

            Text {
                text: root.status;
                color: #55606d;
                font-size: 10px;
                horizontal-alignment: center;
            }
        }

        if root.browser-open: Browser {
            x: 0;
            y: 0;
            width: 100%;
            height: 100%;
            path-text: root.browser-path;
            rows: root.browser-rows;
            pick(i) => { root.browser-pick(i); }
            up => { root.browser-up(); }
            close => { root.browser-close(); }
        }
    }
}

struct DeckState {
    track: Option<Arc<Track>>,
    pyramid: Option<PeakPyramid>,
    syncing: bool,
    looping: bool,
    /// Largo del loop activo en beats; 0 cuando no hay loop.
    loop_beats: f64,
    pitch_pct: f64,
}

struct ViewState {
    zoom_secs: f64,
}

struct BrowserState {
    dir: PathBuf,
    /// Deck que recibirá el track elegido.
    target: usize,
    entries: Vec<PathBuf>,
}

/// Estado del último render de la tira de onda de un deck.
#[derive(Clone, Copy, Default)]
struct WaveRender {
    center: f64,
    zoom: f64,
    rate: f64,
    /// Resolución del bitmap con la que se dibujó, para redibujar si la ventana
    /// cambia de tamaño.
    lane_px: u32,
    lane_py: u32,
    valido: bool,
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--selftest") {
        return run_selftest();
    }
    if let Some(arg) = args.iter().find(|a| a.starts_with("--autoplay")) {
        let secs: u64 = arg.split('=').nth(1).and_then(|s| s.parse().ok()).unwrap_or(10);
        return run_device_test(secs);
    }

    // Proceso de audio: lo lanza la UI con --audio-engine. No abre ventana ni
    // habla con Slint: sólo mezcla y escribe en el dispositivo.
    if args.iter().any(|a| a == "--audio-engine") {
        return run_motor();
    }

    // Una sola instancia: dos procesos de djdeck suman el mismo audio con
    // offsets distintos y eso se escucha como peine de fase ("chispeo").
    let _instancia = match tomar_instancia_unica() {
        Some(lock) => lock,
        None => {
            eprintln!("ya hay una instancia de djdeck corriendo; cerrala antes de abrir otra");
            return Ok(());
        }
    };

    // El motor de audio corre en su PROPIO proceso. Si comparte proceso con el
    // repintado de la ventana, el cliente entrega bloques tarde, PipeWire los
    // descarta y se escucha chispeo: medido 98 % de frames con la UI en el mismo
    // proceso contra 99.98 % con el motor solo. La UI y el motor se hablan por
    // estos atómicos en un archivo mapeado.
    let shm_path = std::env::var("XDG_RUNTIME_DIR")
        .map(|d| format!("{d}/djdeck-shm"))
        .unwrap_or_else(|_| "/tmp/djdeck-shm".to_string());
    let mapping = engine::shm::SharedMapping::create(Path::new(&shm_path))
        .with_context(|| format!("no se pudo crear {shm_path}"))?;
    let mapping: &'static engine::shm::SharedMapping = Box::leak(Box::new(mapping));
    let shared = mapping.block();
    let controls: &'static Controls = &shared.controls;

    let _motor = MotorProceso::lanzar(&shm_path)?;
    let sr = esperar_motor(shared)?;
    let buffer_frames = shared.buffer_frames.load(Ordering::Relaxed);
    let latency_ms = buffer_frames as f64 * 1000.0 / sr as f64;
    println!("motor en proceso aparte | {sr} Hz | {buffer_frames} frames ({latency_ms:.1} ms)");

    let decks = Rc::new(RefCell::new([
        DeckState {
            track: None,
            pyramid: None,
            syncing: false,
            looping: false,
            loop_beats: 0.0,
            pitch_pct: 0.0,
        },
        DeckState {
            track: None,
            pyramid: None,
            syncing: false,
            looping: false,
            loop_beats: 0.0,
            pitch_pct: 0.0,
        },
    ]));
    let view = Rc::new(RefCell::new(ViewState { zoom_secs: 8.0 }));
    let browser = Rc::new(RefCell::new(BrowserState {
        dir: default_music_dir(),
        target: 0,
        entries: Vec::new(),
    }));

    let ui = AppWindow::new()?;
    // Tamaño inicial: el layout es fluido, así que la ventana arranca en el
    // tamaño histórico y de ahí en más manda el usuario (o el WM, si maximiza).
    ui.window()
        .set_size(slint::PhysicalSize::new(1240, 780));
    // DJDECK_FULLSCREEN=1 abre a pantalla completa: sirve cuando la ventana
    // aparece detrás de otras o en un escritorio que no estás mirando.
    if std::env::var("DJDECK_FULLSCREEN").is_ok() {
        ui.window().set_fullscreen(true);
    }

    // ---- carga inicial ----
    let mut loaded: [Option<Arc<Track>>; DECK_COUNT] = [None, None];
    // Lo que se le pide al proceso de audio: una ruta, o `demo:<bpm>:<detune>`
    // para los demos sintéticos (el motor los genera igual que la UI).
    let mut pedidos: [Option<String>; DECK_COUNT] = [None, None];
    if args.is_empty() {
        for i in 0..DECK_COUNT {
            let bpm = if i == 0 { 127.4 } else { 128.0 };
            let detune = if i == 0 { -3.0 } else { 0.0 };
            loaded[i] = Some(Arc::new(make_demo_track(bpm, sr, detune)));
            pedidos[i] = Some(format!("demo:{bpm}:{detune}"));
        }
        println!("sin archivos: uso dos demos sintéticos (127.4 y 128.0 BPM)");
    } else {
        for (i, path) in args.iter().take(DECK_COUNT).enumerate() {
            let p = Path::new(path);
            print!("analizando {} … ", p.display());
            let (track, _pyr) = load_track(p, sr)?;
            println!("{:.1} BPM, primer beat {:.3}s", track.grid.bpm, track.grid.first_beat);
            loaded[i] = Some(track);
            pedidos[i] = Some(path.clone());
        }
    }

    // ---- diagnóstico con la UI andando: `DJDECK_DIAG=<segundos>` ----
    // Reproduce los dos decks y reporta cada 5 s si el callback llega a tiempo.
    // Sin ventana (--autoplay) la UI no compite por CPU, así que no sirve para
    // medir el caso real: acá sí corre el lazo de 30 Hz con las ondas.
    if let Ok(secs) = std::env::var("DJDECK_DIAG") {
        let secs: u64 = secs.parse().unwrap_or(20);
        for d in 0..DECK_COUNT {
            controls.playing[d].store(true, Ordering::Relaxed);
        }
        ui.set_playing_a(true);
        ui.set_playing_b(true);
        let controls = controls.clone();
        std::thread::spawn(move || {
            let t0 = std::time::Instant::now();
            let base = controls.frames_rendered.load(Ordering::Relaxed);
            let mut ultimo = 0.0f64;
            let mut pico_win = [0.0f32; DECK_COUNT];
            let mut pico_master_win = 0.0f32;
            loop {
                std::thread::sleep(std::time::Duration::from_millis(100));
                // master_peak y peak[] son del ÚLTIMO bloque: hay que muestrear
                // seguido y quedarse con el máximo de la ventana.
                for d in 0..DECK_COUNT {
                    pico_win[d] = pico_win[d].max(engine::get_f32(&controls.peak[d]));
                }
                pico_master_win = pico_master_win.max(engine::get_f32(&controls.master_peak));
                let el = t0.elapsed().as_secs_f64();
                if el - ultimo < 5.0 {
                    continue;
                }
                ultimo = el;
                let rend = controls.frames_rendered.load(Ordering::Relaxed) - base;
                let esperado = (el * sr as f64) as u64;
                let pos: Vec<String> = (0..DECK_COUNT)
                    .map(|d| {
                        format!(
                            "{:.2}/{:.1}s",
                            controls.position[d].load(Ordering::Relaxed) as f64 / sr as f64,
                            controls.length[d].load(Ordering::Relaxed) as f64 / sr as f64
                        )
                    })
                    .collect();
                println!(
                    "diag {el:5.1}s | render {:.2} % | xruns {} | pos {} | pico decks {:?} | pico master {:.3}",
                    100.0 * rend as f64 / esperado.max(1) as f64,
                    controls.xruns.load(Ordering::Relaxed),
                    pos.join(" "),
                    pico_win,
                    pico_master_win
                );
                pico_win = [0.0f32; DECK_COUNT];
                pico_master_win = 0.0f32;
                if el as u64 >= secs {
                    std::process::exit(0);
                }
            }
        });
    }

    // ---- foto de la ventana: `DJDECK_SHOT=<ruta .bmp>` ----
    // Una sola captura a los 1500 ms, sin herramienta de pantallazos: alcanza
    // para mirar el layout. Sin la variable no hay hilo ni salida.
    if let Ok(destino) = std::env::var("DJDECK_SHOT") {
        let ui_weak = ui.as_weak();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(1500));
            // `upgrade()` devuelve None fuera del hilo de la UI: la captura se
            // pide al lazo de eventos, que es donde vive la ventana.
            let r = ui_weak.upgrade_in_event_loop(move |ui| match ui.window().take_snapshot() {
                Ok(foto) => {
                    let (w, h) = (foto.width(), foto.height());
                    let bmp = bmp_24(foto.as_slice(), w, h);
                    match std::fs::write(&destino, bmp) {
                        Ok(()) => println!("shot: {destino} {w}x{h}"),
                        Err(e) => println!("shot: error {e}"),
                    }
                }
                Err(e) => println!("shot: error {e}"),
            });
            if let Err(e) = r {
                println!("shot: error {e}");
            }
        });
    }

    for i in 0..DECK_COUNT {
        if let Some(track) = loaded[i].clone() {
            let pyramid = PeakPyramid::build(&track.samples, &[32, 128, 512, 2048, 8192, 32768]);
            if let Some(pedido) = pedidos[i].as_ref() {
                engine::shm::request_load(shared, i, pedido);
            }
            {
                let mut st = decks.borrow_mut();
                st[i].track = Some(track.clone());
                st[i].pyramid = Some(pyramid);
            }
            if i == 0 {
                ui.set_name_a(track.name.clone().into());
                ui.set_bpm_a(format!("{:.1} BPM", track.grid.bpm).into());
            } else {
                ui.set_name_b(track.name.clone().into());
                ui.set_bpm_b(format!("{:.1} BPM", track.grid.bpm).into());
            }
        }
    }

    // ---- callbacks ----
    {
        let controls = controls.clone();
        ui.on_play_a(move || toggle_play(controls, 0));
    }
    {
        let controls = controls.clone();
        ui.on_play_b(move || toggle_play(controls, 1));
    }
    {
        let controls = controls.clone();
        let decks = decks.clone();
        ui.on_cue_a(move || cue(controls, &decks, 0));
    }
    {
        let controls = controls.clone();
        let decks = decks.clone();
        ui.on_cue_b(move || cue(controls, &decks, 1));
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
        ui.on_pitch_a_changed(move |v| set_pitch(controls, &decks, 0, v));
    }
    {
        let controls = controls.clone();
        let decks = decks.clone();
        ui.on_pitch_b_changed(move |v| set_pitch(controls, &decks, 1, v));
    }
    {
        let controls = controls.clone();
        let decks = decks.clone();
        let ui_weak = ui.as_weak();
        ui.on_pitch_reset_a(move || reset_pitch(controls, &decks, &ui_weak, 0));
    }
    {
        let controls = controls.clone();
        let decks = decks.clone();
        let ui_weak = ui.as_weak();
        ui.on_pitch_reset_b(move || reset_pitch(controls, &decks, &ui_weak, 1));
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
        ui.on_eq_low_a_changed(move |v| engine::set_f32(&controls.eq_low[0], v));
    }
    {
        let controls = controls.clone();
        ui.on_eq_low_b_changed(move |v| engine::set_f32(&controls.eq_low[1], v));
    }
    {
        let controls = controls.clone();
        ui.on_eq_mid_a_changed(move |v| engine::set_f32(&controls.eq_mid[0], v));
    }
    {
        let controls = controls.clone();
        ui.on_eq_mid_b_changed(move |v| engine::set_f32(&controls.eq_mid[1], v));
    }
    {
        let controls = controls.clone();
        ui.on_eq_high_a_changed(move |v| engine::set_f32(&controls.eq_high[0], v));
    }
    {
        let controls = controls.clone();
        ui.on_eq_high_b_changed(move |v| engine::set_f32(&controls.eq_high[1], v));
    }
    {
        let controls = controls.clone();
        ui.on_xf_changed(move |v| engine::set_f32(&controls.crossfader, v));
    }
    {
        let controls = controls.clone();
        let decks = decks.clone();
        let ui_weak = ui.as_weak();
        ui.on_loop_a(move |beats| {
            toggle_loop(controls, &decks, &ui_weak, 0, sr, beats as f64)
        });
    }
    {
        let controls = controls.clone();
        let decks = decks.clone();
        let ui_weak = ui.as_weak();
        ui.on_loop_b(move |beats| {
            toggle_loop(controls, &decks, &ui_weak, 1, sr, beats as f64)
        });
    }
    {
        let view = view.clone();
        let ui_weak = ui.as_weak();
        ui.on_zoom_in(move || {
            let mut v = view.borrow_mut();
            v.zoom_secs = (v.zoom_secs / 2.0).max(1.0);
            let text = format!("{} s", v.zoom_secs);
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_zoom_text(text.into());
            }
        });
    }
    {
        let view = view.clone();
        let ui_weak = ui.as_weak();
        ui.on_zoom_out(move || {
            let mut v = view.borrow_mut();
            v.zoom_secs = (v.zoom_secs * 2.0).min(64.0);
            let text = format!("{} s", v.zoom_secs);
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_zoom_text(text.into());
            }
        });
    }

    // DJDECK_PLAY=1 arranca los dos decks y el sync de B contra A: sirve para
    // mirar la alineación de las ondas sin tocar el mouse.
    if std::env::var("DJDECK_PLAY").is_ok() {
        controls.playing[0].store(true, Ordering::Relaxed);
        controls.playing[1].store(true, Ordering::Relaxed);
        decks.borrow_mut()[1].syncing = true;
        ui.set_synced_b(true);
    }

    {
        let ui_weak = ui.as_weak();
        let browser = browser.clone();
        ui.on_browse_a(move || {
            if let Some(ui) = ui_weak.upgrade() {
                browser.borrow_mut().target = 0;
                refresh_browser(&ui, &browser);
                ui.set_browser_open(true);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        let browser = browser.clone();
        ui.on_browse_b(move || {
            if let Some(ui) = ui_weak.upgrade() {
                browser.borrow_mut().target = 1;
                refresh_browser(&ui, &browser);
                ui.set_browser_open(true);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        let browser = browser.clone();
        ui.on_browser_up(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let parent = browser
                .borrow()
                .dir
                .parent()
                .map(|p| p.to_path_buf());
            if let Some(parent) = parent {
                browser.borrow_mut().dir = parent;
                refresh_browser(&ui, &browser);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        ui.on_browser_close(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_browser_open(false);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        let browser = browser.clone();
        let decks = decks.clone();
        let controls = controls.clone();
        ui.on_browser_pick(move |idx| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let (chosen, target) = {
                let st = browser.borrow();
                (st.entries.get(idx.max(0) as usize).cloned(), st.target)
            };
            let Some(chosen) = chosen else { return };

            if chosen.is_dir() {
                browser.borrow_mut().dir = chosen;
                refresh_browser(&ui, &browser);
                return;
            }

            ui.set_status(format!("cargando {} …", chosen.display()).into());
            match load_track(&chosen, sr) {
                Ok((track, pyr)) => {
                    engine::shm::request_load(
                        shared,
                        target,
                        &chosen.to_string_lossy(),
                    );
                    {
                        let mut st = decks.borrow_mut();
                        st[target].track = Some(track.clone());
                        st[target].pyramid = Some(pyr);
                        st[target].syncing = false;
                        st[target].looping = false;
                        st[target].pitch_pct = 0.0;
                    }
                    engine::set_f32(&controls.rate[target], 1.0);
                    controls.loop_on[target].store(false, Ordering::Relaxed);
                    engine::set_f32(&controls.eq_low[target], 0.0);
                    engine::set_f32(&controls.eq_mid[target], 0.0);
                    engine::set_f32(&controls.eq_high[target], 0.0);
                    let name: slint::SharedString = track.name.clone().into();
                    let bpm: slint::SharedString = format!("{:.1} BPM", track.grid.bpm).into();
                    let zero: slint::SharedString = "+0.0%".into();
                    let zero_db: slint::SharedString = "+0.0 dB".into();
                    if target == 0 {
                        ui.set_name_a(name);
                        ui.set_bpm_a(bpm);
                        ui.set_pitch_a_text(zero.clone());
                        ui.set_pitch_a(0.0);
                        ui.set_low_a_text(zero_db.clone());
                        ui.set_mid_a_text(zero_db.clone());
                        ui.set_high_a_text(zero_db);
                        ui.set_synced_a(false);
                        ui.set_looping_a(false);
                    } else {
                        ui.set_name_b(name);
                        ui.set_bpm_b(bpm);
                        ui.set_pitch_b_text(zero.clone());
                        ui.set_pitch_b(0.0);
                        ui.set_low_b_text(zero_db.clone());
                        ui.set_mid_b_text(zero_db.clone());
                        ui.set_high_b_text(zero_db);
                        ui.set_synced_b(false);
                        ui.set_looping_b(false);
                    }
                    ui.set_browser_open(false);
                    ui.set_status(
                        format!(
                            "deck {} ← {} · {:.1} BPM",
                            if target == 0 { "A" } else { "B" },
                            track.name,
                            track.grid.bpm
                        )
                        .into(),
                    );
                }
                Err(e) => {
                    ui.set_status(format!("no se pudo cargar {}: {e}", chosen.display()).into());
                }
            }
        });
    }

    // DJDECK_BROWSER=1 abre el navegador en el deck A al arrancar (para revisar
    // la lista sin tocar el mouse).
    if std::env::var("DJDECK_BROWSER").is_ok() {
        browser.borrow_mut().target = 0;
        refresh_browser(&ui, &browser);
        ui.set_browser_open(true);
    }

    // DJDECK_LOOP=1 activa el loop de 8 beats en A al arrancar (para revisar el
    // estado del botón sin clickear).
    if std::env::var("DJDECK_LOOP").is_ok() {
        let ui_weak = ui.as_weak();
        toggle_loop(controls, &decks, &ui_weak, 0, sr, 8.0);
    }

    ui.set_gain_a(engine::get_f32(&controls.gain[0]));
    ui.set_gain_b(engine::get_f32(&controls.gain[1]));
    ui.set_xf(engine::get_f32(&controls.crossfader));
    ui.set_zoom_text(format!("{} s", view.borrow().zoom_secs).into());
    ui.set_status(
        format!(
            "{} Hz · {} frames ({:.1} ms) · ondas apiladas con rejilla · EQ real de 3 bandas",
            sr, buffer_frames, latency_ms
        )
        .into(),
    );

    // ---- refresco ~30 Hz: sync, ondas, medidores, pitch efectivo ----
    {
        let controls = controls.clone();
        let decks = decks.clone();
        let view = view.clone();
        let ui_weak = ui.as_weak();
        let sr_f = sr as f64;
        let timer = Timer::default();
        // Refresco de la UI. Default 66 ms (15 Hz) y no 33 (30 Hz): sobre
        // escritorio remoto el video de la ventana compite con el audio en el
        // mismo enlace y 30 Hz se escucha como chispeo (medido con RDP: a 1 Hz
        // limpio, a 30 Hz con chispeo, a 15 Hz limpio). En la máquina local
        // DJDECK_UI_MS=33 da el refresco completo sin penalidad.
        //
        // DJDECK_REMOTO=1 baja a 1 Hz: comprobado que con la ventana en segundo
        // plano (sin video que codificar) el audio sale limpio, así que dejar la
        // ventana casi estática es la forma de escuchar por escritorio remoto
        // con la UI a la vista.
        let ui_ms = std::env::var("DJDECK_UI_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or_else(|| {
                if std::env::var("DJDECK_REMOTO").is_ok() {
                    1000
                } else {
                    66
                }
            });
        let wave_state = Rc::new(RefCell::new([WaveRender::default(); DECK_COUNT]));
        let mut tick_n = 0u32;
        let mut tick_max = 0.0f64;
        let mut onda_max = 0.0f64;
        let diag_on = std::env::var("DJDECK_DIAG").is_ok();
        timer.start(TimerMode::Repeated, Duration::from_millis(ui_ms), move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let t_tick = std::time::Instant::now();

            let pos: Vec<f64> = (0..DECK_COUNT)
                .map(|i| controls.position[i].load(Ordering::Relaxed) as f64 / sr_f)
                .collect();
            let len: Vec<f64> = (0..DECK_COUNT)
                .map(|i| controls.length[i].load(Ordering::Relaxed) as f64 / sr_f)
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

            // Pitch efectivo: con sync activo el fader sigue al lazo.
            for i in 0..DECK_COUNT {
                let rate = engine::get_f32(&controls.rate[i]) as f64;
                let pct = (rate - 1.0) * 100.0;
                let (syncing, grid_bpm) = {
                    let mut st = decks.borrow_mut();
                    st[i].pitch_pct = pct;
                    (
                        st[i].syncing,
                        st[i].track.as_ref().map(|t| t.grid.bpm).unwrap_or(0.0),
                    )
                };
                let text = if syncing {
                    format!("SYNC {:+.1}%", pct)
                } else {
                    format!("{:+.1}%", pct)
                };
                let bpm_text = if grid_bpm > 0.0 {
                    if syncing {
                        format!("{:.1} → {:.1}", grid_bpm, grid_bpm * rate)
                    } else {
                        format!("{:.1} BPM", grid_bpm)
                    }
                } else {
                    "—".to_string()
                };
                if i == 0 {
                    ui.set_pitch_a_text(text.into());
                    ui.set_pitch_a(pct as f32);
                    ui.set_bpm_a(bpm_text.into());
                } else {
                    ui.set_pitch_b_text(text.into());
                    ui.set_pitch_b(pct as f32);
                    ui.set_bpm_b(bpm_text.into());
                }
            }

            // Ondas: se dibuja una tira de STRIP carriles centrada en la posición
            // y cada tick sólo se DESPLAZA. Redibujar la imagen 30 veces por
            // segundo es lo que ensuciaba el audio (bisección con el binario
            // `probe`: la etapa de la imagen es donde aparece el chispeo, con el
            // contador de frames igual de sano). Ahora la imagen se rehace ~2
            // veces por segundo y el scroll lo hace el offset.
            // DJDECK_NO_WAVE=1 la apaga del todo (diagnóstico).
            let t_onda = std::time::Instant::now();
            if std::env::var("DJDECK_NO_WAVE").is_err() {
                let zoom = view.borrow().zoom_secs;
                let rate = [
                    engine::get_f32(&controls.rate[0]) as f64,
                    engine::get_f32(&controls.rate[1]) as f64,
                ];
                // La ventana es redimensionable: el ancho sale del layout, no de
                // una constante de diseño. Si la onda se mapeara con LANE_W, al
                // maximizar el playhead dejaría de caer en el centro del carril.
                let lane = [
                    (ui.get_lane_a_w() as f64, ui.get_lane_a_h() as f64),
                    (ui.get_lane_b_w() as f64, ui.get_lane_b_h() as f64),
                ];
                let mut ws = wave_state.borrow_mut();
                for i in 0..DECK_COUNT {
                    let (lane_w, lane_h) = lane[i];
                    // Antes del primer layout todavía no hay carril que dibujar.
                    if lane_w < 80.0 || lane_h < 40.0 {
                        continue;
                    }
                    let (px_por_frame, frames_per_px) = wave_scale(zoom, sr_f, lane_w);
                    let (lane_px, lane_py) = strip_size(lane_w, lane_h);
                    let src_per_px = frames_per_px * rate[i];
                    let ahora = pos[i] * sr_f;
                    let necesita = !ws[i].valido
                        || (ws[i].zoom - zoom).abs() > 1e-9
                        || (ws[i].rate - rate[i]).abs() > 1e-9
                        || ws[i].lane_px != lane_px
                        || ws[i].lane_py != lane_py
                        || ((ahora - ws[i].center) / src_per_px).abs() > lane_px as f64 / 2.0;
                    if necesita {
                        let st = decks.borrow();
                        if let Some(pyr) = st[i].pyramid.as_ref() {
                            let img = render_lane(
                                pyr,
                                ahora,
                                frames_per_px,
                                rate[i],
                                if i == 0 { (34, 211, 238) } else { (251, 191, 36) },
                                st[i].track.as_ref().map(|t| t.grid),
                                sr_f,
                                lane_px,
                                lane_py,
                            );
                            ws[i] = WaveRender {
                                center: ahora,
                                zoom,
                                rate: rate[i],
                                lane_px,
                                lane_py,
                                valido: true,
                            };
                            drop(st);
                            if i == 0 {
                                ui.set_wave_a(img);
                            } else {
                                ui.set_wave_b(img);
                            }
                        }
                    }
                    // La tira se dibujó centrada en ws[i].center. Si el deck
                    // avanzó, ese punto quedó a la derecha del centro de la
                    // tira: hay que correr la imagen hacia la IZQUIERDA (x más
                    // negativo) para que caiga en el centro del carril.
                    let deriva = if ws[i].valido {
                        ((ahora - ws[i].center) * px_por_frame) as f32
                    } else {
                        0.0
                    };
                    let x = -(lane_w as f32) - deriva;
                    if i == 0 {
                        ui.set_wave_a_x(x);
                    } else {
                        ui.set_wave_b_x(x);
                    }

                    // Sombra del loop. Los bordes están en frames del
                    // dispositivo y el eje visible avanza `px_por_frame` píxeles
                    // de pantalla por frame. Con el playhead al centro, un punto
                    // fijo del track se corre a la izquierda a medida que el deck
                    // avanza.
                    let sombra = if controls.loop_on[i].load(Ordering::Relaxed) {
                        loop_shadow(
                            controls.loop_start[i].load(Ordering::Relaxed) as f64,
                            controls.loop_end[i].load(Ordering::Relaxed) as f64,
                            ahora,
                            px_por_frame,
                            lane_w,
                        )
                    } else {
                        None
                    };
                    let (sx, sw) = sombra.unwrap_or((0.0, 0.0));
                    if i == 0 {
                        ui.set_loop_a_visible(sombra.is_some());
                        ui.set_loop_a_x(sx);
                        ui.set_loop_a_w(sw);
                    } else {
                        ui.set_loop_b_visible(sombra.is_some());
                        ui.set_loop_b_x(sx);
                        ui.set_loop_b_w(sw);
                    }
                }
            }

            let peaks = [
                engine::get_f32(&controls.peak[0]),
                engine::get_f32(&controls.peak[1]),
            ];
            onda_max = onda_max.max(t_onda.elapsed().as_secs_f64() * 1000.0);

            ui.set_meter_a((peaks[0] * 5.0).min(1.0));
            ui.set_meter_b((peaks[1] * 5.0).min(1.0));
            ui.set_meter_master((engine::get_f32(&controls.master_peak) * 5.0).min(1.0));
            ui.set_playing_a(controls.playing[0].load(Ordering::Relaxed));
            ui.set_playing_b(controls.playing[1].load(Ordering::Relaxed));
            ui.set_time_a(fmt_time(pos[0], len[0]).into());
            ui.set_time_b(fmt_time(pos[1], len[1]).into());

            // Costo del tick: si se acerca al período del buffer, el callback de
            // audio pierde el turno y se cae un bloque (chasquido).
            tick_n += 1;
            tick_max = tick_max.max(t_tick.elapsed().as_secs_f64() * 1000.0);
            if diag_on && tick_n % 150 == 0 {
                println!(
                    "ui: tick máx {:.1} ms | ondas máx {:.1} ms (período del buffer 21.3 ms)",
                    tick_max, onda_max
                );
                tick_max = 0.0;
                onda_max = 0.0;
            }

            let gains = [
                engine::get_f32(&controls.gain[0]),
                engine::get_f32(&controls.gain[1]),
            ];
            ui.set_gain_a_text(format!("{:.0}%", gains[0] * 100.0).into());
            ui.set_gain_b_text(format!("{:.0}%", gains[1] * 100.0).into());

            for i in 0..DECK_COUNT {
                let (lo, mi, hi) = (
                    engine::get_f32(&controls.eq_low[i]),
                    engine::get_f32(&controls.eq_mid[i]),
                    engine::get_f32(&controls.eq_high[i]),
                );
                let (a, b, c) = (
                    format!("{:+.1} dB", lo),
                    format!("{:+.1} dB", mi),
                    format!("{:+.1} dB", hi),
                );
                if i == 0 {
                    ui.set_low_a_text(a.into());
                    ui.set_mid_a_text(b.into());
                    ui.set_high_a_text(c.into());
                } else {
                    ui.set_low_b_text(a.into());
                    ui.set_mid_b_text(b.into());
                    ui.set_high_b_text(c.into());
                }
            }
        });
        std::mem::forget(timer); // vive lo que vive la app
    }

    ui.run()?;
    Ok(())
}

const AUDIO_EXT: [&str; 9] = ["mp3", "flac", "wav", "m4a", "aac", "ogg", "opus", "aif", "aiff"];

fn is_audio(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| AUDIO_EXT.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

/// Carpeta inicial del navegador: `DJDECK_MUSIC`, si no Downloads, si no Música.
fn default_music_dir() -> PathBuf {
    if let Ok(d) = std::env::var("DJDECK_MUSIC") {
        let p = PathBuf::from(d);
        if p.is_dir() {
            return p;
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    for cand in ["Downloads", "Music", "Música"] {
        let p = PathBuf::from(&home).join(cand);
        if p.is_dir() {
            return p;
        }
    }
    PathBuf::from(home)
}

/// Candado de instancia única: dos djdeck a la vez suman el mismo audio con
/// offsets distintos (peine de fase) y suenan a chispeo. Si el PID guardado ya
/// no existe, el candado se considera viejo y se toma.
struct InstanceLock {
    path: PathBuf,
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn tomar_instancia_unica() -> Option<InstanceLock> {
    let path = std::env::var("XDG_RUNTIME_DIR")
        .map(|d| PathBuf::from(d).join("djdeck.lock"))
        .unwrap_or_else(|_| std::env::temp_dir().join("djdeck.lock"));
    if let Ok(txt) = std::fs::read_to_string(&path) {
        if let Ok(pid) = txt.trim().parse::<u32>() {
            if Path::new(&format!("/proc/{pid}")).exists() {
                return None;
            }
        }
    }
    std::fs::write(&path, std::process::id().to_string()).ok()?;
    Some(InstanceLock { path })
}

/// Decodifica, remuestrea, detecta BPM y arma la pirámide de picos.
fn load_track(path: &Path, sr: u32) -> Result<(Arc<Track>, PeakPyramid)> {
    let decoded = engine::decode_file(path).with_context(|| format!("leyendo {}", path.display()))?;
    let samples = engine::resample_stereo(&decoded.samples, decoded.sample_rate, sr);
    let (bpm, first) = analysis::detect_bpm(&samples, sr)
        .ok_or_else(|| anyhow::anyhow!("no se pudo detectar el BPM"))?;
    let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("track");
    let track = Arc::new(Track::new(name, samples, sr, BeatGrid::new(bpm, first)));
    let pyramid = PeakPyramid::build(&track.samples, &[32, 128, 512, 2048, 8192, 32768]);
    Ok((track, pyramid))
}

/// Relee la carpeta actual: carpetas primero, después archivos de audio.
fn refresh_browser(ui: &AppWindow, browser: &Rc<RefCell<BrowserState>>) {
    let dir = browser.borrow().dir.clone();
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut files: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
                if !name.starts_with('.') {
                    dirs.push(path);
                }
            } else if is_audio(&path) {
                files.push(path);
            }
        }
    }
    dirs.sort_by_key(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default());
    files.sort_by_key(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default());

    let mut all = dirs;
    all.extend(files);

    let rows: Vec<BrowserRow> = all
        .iter()
        .map(|p| BrowserRow {
            name: p
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default()
                .into(),
            is_dir: p.is_dir(),
        })
        .collect();

    browser.borrow_mut().entries = all;
    ui.set_browser_path(dir.display().to_string().into());
    ui.set_browser_rows(ModelRc::from(Rc::new(VecModel::from(rows))));
}

fn toggle_play(controls: &Controls, deck: usize) {
    let cur = controls.playing[deck].load(Ordering::Relaxed);
    controls.playing[deck].store(!cur, Ordering::Relaxed);
}

fn cue(controls: &Controls, decks: &Rc<RefCell<[DeckState; DECK_COUNT]>>, deck: usize) {
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

/// Ventana de loop de `beats` beats, enganchada al beat que contiene `pos`
/// (frames del dispositivo). Devuelve (inicio, fin) en frames, recortado al
/// final del track, o `None` si el rango quedaría degenerado.
fn loop_window(
    beat_len: f64,
    first_beat: f64,
    pos: f64,
    sr: u32,
    frames: f64,
    beats: f64,
) -> Option<(f64, f64)> {
    let beat_frames = beat_len * sr as f64;
    if beat_frames < 2.0 || beats < 1.0 {
        return None;
    }
    let rel_beats = (pos / sr as f64 - first_beat) / beat_len;
    let beat_k = rel_beats.floor().max(0.0);
    let start = ((first_beat + beat_k * beat_len) * sr as f64).max(0.0);
    let end = (start + beats * beat_frames).min(frames - 1.0);
    if end <= start + 2.0 {
        return None;
    }
    Some((start, end))
}

/// Rectángulo en píxeles de pantalla que ocupa el loop sobre el carril de onda.
/// El carril muestra la posición actual en su centro y la tira se corre a la
/// izquierda a medida que el deck avanza, así que un tramo fijo del track se
/// desplaza: `x` se recorta al carril y `w` nunca queda negativa.
fn loop_shadow(
    loop_start: f64,
    loop_end: f64,
    ahora: f64,
    px_por_frame: f64,
    lane_w: f64,
) -> Option<(f32, f32)> {
    if px_por_frame <= 0.0 || loop_end <= loop_start {
        return None;
    }
    let medio = lane_w / 2.0;
    let x0 = medio + (loop_start - ahora) * px_por_frame;
    let x1 = medio + (loop_end - ahora) * px_por_frame;
    let x = x0.max(0.0);
    let w = x1.min(lane_w) - x;
    if w < 1.0 {
        return None;
    }
    Some((x as f32, w as f32))
}

/// Escala del eje de tiempo del carril visible: (píxeles de pantalla por frame
/// del dispositivo, frames por píxel del bitmap). Depende del ancho REAL del
/// carril y no de la resolución del bitmap: por eso al maximizar la ventana la
/// onda sigue alineada con el playhead y con la sombra del loop.
fn wave_scale(zoom: f64, sr: f64, lane_w: f64) -> (f64, f64) {
    let px_por_frame = lane_w / (zoom * sr);
    let frames_por_px = zoom * sr / (lane_w / WAVE_DIV as f64);
    (px_por_frame, frames_por_px)
}

/// Resolución del bitmap de una tira de onda: (ancho de un carril, alto) en
/// píxeles, con la misma densidad que el carril en pantalla (1 píxel de bitmap
/// cada WAVE_DIV de pantalla). Con tamaño fijo, maximizar estiraba el bitmap y
/// la onda se veía borrosa. Los topes evitan que una ventana enorme dispare el
/// costo del redibujado, que es todo el costo del tick de UI.
fn strip_size(lane_w: f64, lane_h: f64) -> (u32, u32) {
    let w = (lane_w / WAVE_DIV as f64).round() as i64;
    let h = (lane_h / WAVE_DIV as f64).round() as i64;
    (
        w.clamp((LANE_W / WAVE_DIV) as i64, 4096) as u32,
        h.clamp(LANE_MIN_PY, 1024) as u32,
    )
}

/// LOOP 4 / LOOP 8: engancha `beats` beats desde el beat que contiene la
/// posición actual. Al activarse, el motor envuelve dentro del rango con un
/// fade de 2 ms. Con un loop activo, pulsar el mismo largo lo apaga y pulsar
/// el otro largo lo reemplaza sin pasar por apagado.
fn toggle_loop(
    controls: &Controls,
    decks: &Rc<RefCell<[DeckState; DECK_COUNT]>>,
    ui: &slint::Weak<AppWindow>,
    deck: usize,
    sr: u32,
    beats: f64,
) {
    let mut st = decks.borrow_mut();
    let mut on;
    if st[deck].looping && st[deck].loop_beats == beats {
        st[deck].looping = false;
        st[deck].loop_beats = 0.0;
        controls.loop_on[deck].store(false, Ordering::Relaxed);
        on = false;
    } else {
        let Some(track) = st[deck].track.clone() else {
            return;
        };
        let pos = controls.position[deck].load(Ordering::Relaxed) as f64;
        let Some((start, end)) = loop_window(
            track.grid.beat_len(),
            track.grid.first_beat,
            pos,
            sr,
            track.frames() as f64,
            beats,
        ) else {
            return;
        };
        controls.loop_start[deck].store(start as u64, Ordering::Relaxed);
        controls.loop_end[deck].store(end as u64, Ordering::Relaxed);
        controls.loop_on[deck].store(true, Ordering::Relaxed);
        st[deck].looping = true;
        st[deck].loop_beats = beats;
        on = true;
    }
    drop(st);
    if let Some(ui) = ui.upgrade() {
        let largo = if on { beats.round() as i32 } else { 0 };
        if deck == 0 {
            ui.set_looping_a(on);
            ui.set_loop_beats_a(largo);
        } else {
            ui.set_looping_b(on);
            ui.set_loop_beats_b(largo);
        }
    }
}

fn set_pitch(controls: &Controls, decks: &Rc<RefCell<[DeckState; DECK_COUNT]>>, deck: usize, pct: f32) {
    {
        let mut st = decks.borrow_mut();
        st[deck].pitch_pct = pct as f64;
        if st[deck].syncing {
            return; // mientras está sincronizado manda el lazo de sync
        }
    }
    engine::set_f32(&controls.rate[deck], 1.0 + pct / 100.0);
}

/// Doble click en el fader de pitch: vuelve al 0 %.
/// Con SYNC activo el lazo reescribe el rate a 30 Hz y el 0 % no dura un frame,
/// así que el reset apaga el sync de ese deck primero.
fn reset_pitch(
    controls: &Controls,
    decks: &Rc<RefCell<[DeckState; DECK_COUNT]>>,
    ui: &slint::Weak<AppWindow>,
    deck: usize,
) {
    if decks.borrow()[deck].syncing {
        toggle_sync(decks, ui, deck);
    }
    set_pitch(controls, decks, deck, 0.0);
}

fn fmt_time(pos: f64, len: f64) -> String {
    fn mmss(s: f64) -> String {
        let s = s.max(0.0);
        format!("{}:{:02}", (s / 60.0) as u64, (s % 60.0) as u64)
    }
    format!("{} / {}", mmss(pos), mmss(len))
}

/// Dibuja una lane: envolvente de picos, rejilla de beats y playhead al centro.
/// Las dos lanes comparten eje de tiempo, así que con el sync enganchado los
/// kicks caen en la misma vertical.
fn render_lane(
    pyr: &PeakPyramid,
    center_frame: f64,
    frames_per_px: f64,
    rate: f64,
    rgb: (u8, u8, u8),
    grid: Option<BeatGrid>,
    sr: f64,
    lane_px: u32,
    lane_py: u32,
) -> Image {
    // Tira de STRIP carriles de ancho: el `Image` se dibuja 3 veces más ancho que
    // el carril y se desplaza, así redibujar es cosa de ~2 veces por segundo.
    // El tamaño sale del carril real: al maximizar la ventana la onda se redibuja
    // a esa resolución en vez de estirar un bitmap chico.
    let (w, h) = (lane_px * STRIP, lane_py);
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(w, h);
    let mid_y = h as f32 / 2.0;
    let mid_x = w as f32 / 2.0;
    // Un píxel es tiempo de reproducción: el material avanza `rate` frames de
    // fuente por frame de salida. Sin esto, dos temas sincronizados a BPM
    // distintos muestran rejillas con espaciado distinto y el sync parece roto.
    let src_per_px = frames_per_px * rate;
    let half_win = mid_x as f64 * src_per_px;
    let gain = 0.92 / pyr.norm;

    {
        let pixels = buf.make_mut_slice();
        for p in pixels.iter_mut() {
            *p = Rgba8Pixel {
                r: 7,
                g: 10,
                b: 14,
                a: 255,
            };
        }

        if let Some(level) = pyr.level_for(src_per_px) {
            let fpb = level.frames_per_bucket as f64;
            let buckets = level.max.len();
            for x in 0..w {
                let frame = center_frame + (x as f64 - mid_x as f64) * src_per_px;
                let b = (frame / fpb) as isize;
                if b < 0 || b as usize >= buckets {
                    continue;
                }
                let hi = level.max[b as usize] * gain;
                let lo = level.min[b as usize] * gain;
                let y0 = (mid_y - hi.clamp(0.0, 1.0) * mid_y).clamp(0.0, h as f32 - 1.0);
                let y1 = (mid_y - lo.clamp(-1.0, 0.0) * mid_y).clamp(0.0, h as f32 - 1.0);
                let (top, bot) = if y0 <= y1 {
                    (y0 as u32, y1 as u32)
                } else {
                    (y1 as u32, y0 as u32)
                };
                for y in top..=bot {
                    let dist = (y as f32 - mid_y).abs() / mid_y.max(1.0);
                    let f = 1.0 - 0.5 * dist;
                    let idx = (y * w + x) as usize;
                    pixels[idx] = Rgba8Pixel {
                        r: (rgb.0 as f32 * f) as u8,
                        g: (rgb.1 as f32 * f) as u8,
                        b: (rgb.2 as f32 * f) as u8,
                        a: 255,
                    };
                }
            }
        }

        // Rejilla: un tick por beat, más marcado cada 4.
        if let Some(grid) = grid {
            let beat_frames = grid.beat_len() * sr;
            if beat_frames > 1.0 {
                let first = grid.first_beat * sr;
                let k0 = ((center_frame - half_win - first) / beat_frames).floor() as i64;
                let k1 = ((center_frame + half_win - first) / beat_frames).ceil() as i64;
                for k in k0..=k1 {
                    if k < 0 {
                        continue;
                    }
                    let beat_frame = first + k as f64 * beat_frames;
                    let x = mid_x as f64 + (beat_frame - center_frame) / src_per_px;
                    if x < 0.0 || x >= w as f64 - 1.0 {
                        continue;
                    }
                    let is_bar = k % 4 == 0;
                    let (alpha, thick) = if is_bar { (0.5f32, 2u32) } else { (0.18, 1) };
                    for dx in 0..thick {
                        let xi = x as u32 + dx;
                        if xi >= w {
                            continue;
                        }
                        for y in 0..h {
                            let idx = (y * w + xi) as usize;
                            let p = pixels[idx];
                            pixels[idx] = Rgba8Pixel {
                                r: (p.r as f32 * (1.0 - alpha) + 255.0 * alpha * 0.55) as u8,
                                g: (p.g as f32 * (1.0 - alpha) + 255.0 * alpha * 0.75) as u8,
                                b: (p.b as f32 * (1.0 - alpha) + 255.0 * alpha) as u8,
                                a: 255,
                            };
                        }
                    }
                }
            }
        }
    }

    Image::from_rgba8(buf)
}

/// Verificación sin UI ni placa de sonido: dos demos, deck B sincronizado a A.
fn run_selftest() -> Result<()> {
    let sr = 48_000u32;
    let controls = Arc::new(Controls::default());
    let pending: PendingLoads = Arc::new(Mutex::new(Vec::new()));
    let mut mixer = engine::Mixer::new(controls.clone(), pending.clone(), sr);

    let a = Arc::new(make_demo_track(127.4, sr, -3.0));
    let b = Arc::new(make_demo_track(128.0, sr, 0.0));
    let grid_a = BeatGrid::new(a.grid.bpm, 0.0);
    let grid_b = BeatGrid::new(b.grid.bpm, 0.0);

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
    let blocks_per_tick = 9;
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

/// Verificación de punta a punta con la placa real, sin UI.
fn run_device_test(secs: u64) -> Result<()> {
    let controls = Arc::new(Controls::default());
    let pending: PendingLoads = Arc::new(Mutex::new(Vec::new()));
    let out = engine::output::start(controls.clone(), pending.clone(), DEFAULT_FRAMES)?;
    let sr = out.sample_rate;
    println!("autoplay: {} | {} Hz | {} frames", out.device_name, sr, out.buffer_frames);

    let a = Arc::new(make_demo_track(127.4, sr, -3.0));
    let b = Arc::new(make_demo_track(128.0, sr, 0.0));
    let (grid_a, grid_b) = (a.grid, b.grid);
    pending.lock().unwrap().push((0, a));
    pending.lock().unwrap().push((1, b));

    std::thread::sleep(Duration::from_millis(300));

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
    println!(
        "autoplay: posiciones A={} B={} frames ({:.2} s de audio) | bloque máximo {} frames",
        pos_a,
        pos_b,
        pos_b as f64 / sr_f,
        controls.block_frames_max.load(Ordering::Relaxed)
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

/// Track sintético: kick en cada beat, hat en la corchea y bajo.
/// Pasa-banda RBJ (0 dB en el pico) para el hat del demo: f0 9 kHz con Q 1.4,
/// o sea unos 6-12 kHz de banda. Con b1 = 0 devuelve (b0, b2, a1, a2).
fn hat_bandpass_coeffs(sr: f32) -> (f32, f32, f32, f32) {
    let (f0, q) = (9_000.0f32, 1.4f32);
    let w0 = 2.0 * std::f32::consts::PI * f0 / sr;
    let (sin, cos) = (w0.sin(), w0.cos());
    let alpha = sin / (2.0 * q);
    let a0 = 1.0 + alpha;
    (alpha / a0, -alpha / a0, (-2.0 * cos) / a0, (1.0 - alpha) / a0)
}

fn make_demo_track(bpm: f64, sr: u32, detune_semitones: f64) -> Track {
    let seconds = 32.0;
    let frames = (seconds * sr as f64) as usize;
    let mut samples = vec![0.0f32; frames * 2];
    let beat = 60.0 / bpm;
    let root = 55.0 * 2f64.powf(detune_semitones / 12.0);

    let mut t = 0.0;
    let mut beat_idx = 0usize;
    while t < seconds {
        let start = (t * sr as f64) as usize;

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

        let hat_start = start + (beat * 0.5 * sr as f64) as usize;
        let hat_len = (sr as f64 * 0.04) as usize;
        // Hat de banda limitada: ruido pasado por un pasa-banda 6-12 kHz, con
        // una semilla por canal y paneo alternado. El ruido blanco crudo tenia
        // energia plana hasta Nyquist (siseo) y los dos canales identicos.
        let (b0, b2, a1, a2) = hat_bandpass_coeffs(sr as f32);
        let pan = if beat_idx % 2 == 0 { 0.18f32 } else { -0.18f32 };
        let (gan_l, gan_r) = (1.0 - pan * 0.5, 1.0 + pan * 0.5);
        let mut canales = [vec![0.0f32; hat_len], vec![0.0f32; hat_len]];
        for (ch, buf) in canales.iter_mut().enumerate() {
            let mut seed = 12345u32
                .wrapping_add(beat_idx as u32 * 7919)
                .wrapping_add(ch as u32 * 1_048_573);
            let (mut z0, mut z1) = (0.0f32, 0.0f32);
            for (i, out) in buf.iter_mut().enumerate() {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                let noise = ((seed >> 9) as f32 / (1 << 23) as f32) * 2.0 - 1.0;
                let y = b0 * noise + z0;
                z0 = -a1 * y + z1;
                z1 = b2 * noise - a2 * y;
                let env = (-(i as f32) / sr as f32 * 60.0).exp();
                *out = y * env;
            }
        }
        let pico_hat = canales[0]
            .iter()
            .chain(canales[1].iter())
            .fold(0.0f32, |m, v| m.max(v.abs()));
        let escala = if pico_hat > 0.0 { 0.22 / pico_hat } else { 0.0 };
        for i in 0..hat_len {
            let idx = hat_start + i;
            if idx >= frames {
                break;
            }
            samples[idx * 2] += canales[0][i] * escala * gan_l;
            samples[idx * 2 + 1] += canales[1][i] * escala * gan_r;
        }

        let pattern = [0, 0, 7, 5];
        let semis = pattern[beat_idx % pattern.len()] as f64;
        let f = root * 2f64.powf(semis / 12.0);
        let bass_len = (beat * sr as f64) as usize;
        for i in 0..bass_len {
            let idx = start + i;
            if idx >= frames {
                break;
            }
            let tt = i as f64 / sr as f64;
            // El bajo llena el beat completo: con 0.9 beat quedaban 47 ms de
            // silencio exacto antes de cada beat y el demo tartamudeaba.
            let env = (-tt * 4.0).exp() * (1.0 - tt / beat).max(0.0);
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

// ---------------------------------------------------------------------------
// Proceso de audio dedicado
// ---------------------------------------------------------------------------

/// Mantiene vivo al proceso de audio y lo mata cuando la UI termina.
struct MotorProceso {
    hijo: std::process::Child,
}

impl MotorProceso {
    fn lanzar(shm_path: &str) -> Result<Self> {
        let exe = std::env::current_exe()?;
        let hijo = std::process::Command::new(exe)
            .arg("--audio-engine")
            .env("DJDECK_SHM", shm_path)
            .env("DJDECK_PARENT", std::process::id().to_string())
            .spawn()
            .context("no se pudo lanzar el proceso de audio")?;
        Ok(Self { hijo })
    }
}

impl Drop for MotorProceso {
    fn drop(&mut self) {
        let _ = self.hijo.kill();
        let _ = self.hijo.wait();
    }
}

/// Espera a que el motor publique el SR del dispositivo que abrió.
fn esperar_motor(shared: &engine::shm::Shared) -> Result<u32> {
    for _ in 0..600 {
        let sr = shared.sample_rate.load(Ordering::Relaxed);
        if sr != 0 {
            return Ok(sr);
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    Err(anyhow::anyhow!(
        "el proceso de audio no abrió el dispositivo (¿lo está usando otra app?)"
    ))
}

/// Proceso de audio: sin ventana, sin Slint. Sólo mezcla y escribe al dispositivo.
fn run_motor() -> Result<()> {
    let shm_path = std::env::var("DJDECK_SHM").context("DJDECK_SHM no definido")?;
    let mapping = engine::shm::SharedMapping::attach(Path::new(&shm_path))
        .with_context(|| format!("no se pudo mapear {shm_path}"))?;
    let mapping: &'static engine::shm::SharedMapping = Box::leak(Box::new(mapping));
    let shared = mapping.block();
    let controls: &'static Controls = &shared.controls;
    let pending: PendingLoads = Arc::new(Mutex::new(Vec::new()));

    let out = engine::output::start(controls, pending.clone(), DEFAULT_FRAMES)?;
    let sr = out.sample_rate;
    shared.sample_rate.store(sr, Ordering::Relaxed);
    shared.buffer_frames.store(out.buffer_frames, Ordering::Relaxed);
    println!(
        "audio: {} | {sr} Hz | {} canales | {} frames | formato {:?}",
        out.device_name, out.channels, out.buffer_frames, out.sample_format
    );

    {
        let pending = pending.clone();
        std::thread::spawn(move || loop {
            for d in 0..DECK_COUNT {
                if let Some(pedido) = engine::shm::take_load(shared, d) {
                    match cargar_para_motor(&pedido, sr) {
                        Ok(track) => {
                            println!("audio: deck {} <- {}", d, track.name);
                            pending.lock().unwrap().push((d, track));
                        }
                        Err(e) => eprintln!("audio: no se pudo cargar {pedido}: {e}"),
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        });
    }

    // Si la UI desaparece, el motor también: no queda un proceso sonando solo.
    let padre: u32 = std::env::var("DJDECK_PARENT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if padre > 0 {
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(2));
            if !Path::new(&format!("/proc/{padre}")).exists() {
                std::process::exit(0);
            }
        });
    }

    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

/// Atiende un pedido de carga: ruta de archivo o `demo:<bpm>:<detune>`.
fn cargar_para_motor(pedido: &str, sr: u32) -> Result<Arc<Track>> {
    if let Some(resto) = pedido.strip_prefix("demo:") {
        let mut it = resto.split(':');
        let bpm: f64 = it.next().unwrap_or("120").parse().unwrap_or(120.0);
        let detune: f64 = it.next().unwrap_or("0").parse().unwrap_or(0.0);
        return Ok(Arc::new(make_demo_track(bpm, sr, detune)));
    }
    let path = Path::new(pedido);
    let decoded =
        engine::decode_file(path).with_context(|| format!("leyendo {}", path.display()))?;
    let samples = engine::resample_stereo(&decoded.samples, decoded.sample_rate, sr);
    let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("track");
    Ok(Arc::new(Track::new(
        name,
        samples,
        sr,
        BeatGrid::new(0.0, 0.0),
    )))
}

/// Arma un BMP de 24 bits sin comprimir a mano: 14 bytes de BITMAPFILEHEADER +
/// 40 de BITMAPINFOHEADER, filas de abajo hacia arriba y cada fila rellenada a
/// múltiplo de 4 bytes. Entra RGBA y sale BGR porque el BMP no lleva alfa; se
/// hace acá para no sumar una dependencia de imágenes.
fn bmp_24(rgba: &[Rgba8Pixel], w: u32, h: u32) -> Vec<u8> {
    let fila = (w as usize * 3).div_ceil(4) * 4; // relleno a 4 bytes por fila
    let imagen = fila * h as usize;
    let mut out = Vec::with_capacity(54 + imagen);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&((54 + imagen) as u32).to_le_bytes()); // tamaño del archivo
    out.extend_from_slice(&0u32.to_le_bytes()); // reservado
    out.extend_from_slice(&54u32.to_le_bytes()); // offset de los píxeles
    out.extend_from_slice(&40u32.to_le_bytes()); // tamaño del BITMAPINFOHEADER
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(h as i32).to_le_bytes()); // alto positivo: abajo hacia arriba
    out.extend_from_slice(&1i16.to_le_bytes()); // planos
    out.extend_from_slice(&24i16.to_le_bytes()); // bits por píxel
    out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB: sin compresión
    out.extend_from_slice(&(imagen as u32).to_le_bytes());
    out.extend_from_slice(&2835i32.to_le_bytes()); // 72 ppp en X
    out.extend_from_slice(&2835i32.to_le_bytes()); // 72 ppp en Y
    out.extend_from_slice(&0u32.to_le_bytes()); // colores en la paleta
    out.extend_from_slice(&0u32.to_le_bytes()); // colores importantes
    let relleno = [0u8; 3];
    for y in (0..h).rev() {
        let base = y as usize * w as usize;
        for px in rgba[base..base + w as usize].iter() {
            out.extend_from_slice(&[px.b, px.g, px.r]);
        }
        out.extend_from_slice(&relleno[..fila - w as usize * 3]);
    }
    out
}

#[cfg(test)]
mod demo_tests {
    use super::*;

    const SR: u32 = 48_000;

    /// Energía en una frecuencia por Goertzel, con ventana de Hann.
    fn goertzel_db(x: &[f32], f: f64) -> f64 {
        let n = x.len();
        let w = 2.0 * std::f64::consts::PI * f / SR as f64;
        let coeff = 2.0 * w.cos();
        let (mut s1, mut s2) = (0.0f64, 0.0f64);
        for (i, v) in x.iter().enumerate() {
            let hann = 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / n as f64).cos();
            let s0 = *v as f64 * hann + coeff * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
        10.0 * (power / n as f64).log10()
    }

    fn canal(demo: &Track, desde_frame: usize, frames: usize, ch: usize) -> Vec<f32> {
        (desde_frame..desde_frame + frames)
            .map(|i| demo.samples[i * 2 + ch])
            .collect()
    }

    /// El hat cae a medio beat del primer tiempo. El kick dura 180 ms y el hat
    /// arranca en beat*0.5 (234 ms a 128 BPM), así que la ventana es sólo hat
    /// más el bajo, que vive por debajo de 110 Hz.
    fn ventana_del_hat(demo: &Track) -> (usize, usize) {
        let beat = 60.0 / demo.grid.bpm;
        let start = (beat * 0.5 * SR as f64) as usize;
        (start, (SR as f64 * 0.04) as usize)
    }

    #[test]
    fn el_hat_del_demo_esta_limitado_a_su_banda() {
        // Ruido blanco crudo = energía plana hasta Nyquist: suena a siseo. Un
        // hat de verdad vive en 6-12 kHz. El test compara 9 kHz contra 2 kHz
        // (por debajo de la banda) y 16 kHz (por encima).
        let demo = make_demo_track(128.0, SR, 0.0);
        let (start, len) = ventana_del_hat(&demo);
        let hat = canal(&demo, start, len, 0);
        let db_2k = goertzel_db(&hat, 2_000.0);
        let db_9k = goertzel_db(&hat, 9_000.0);
        let db_16k = goertzel_db(&hat, 16_000.0);
        assert!(
            db_9k > db_2k + 10.0,
            "el hat debe tener 10 dB menos en 2 kHz que en 9 kHz: 2 kHz {db_2k:.1} dB, 9 kHz {db_9k:.1} dB"
        );
        assert!(
            db_9k > db_16k + 6.0,
            "el hat debe tener 6 dB menos en 16 kHz que en 9 kHz: 9 kHz {db_9k:.1} dB, 16 kHz {db_16k:.1} dB"
        );
    }

    #[test]
    fn el_demo_no_tiene_huecos_de_silencio() {
        // Medido con el tap de la app: entre el fin del bajo (0.9 beat) y el
        // beat siguiente quedaban 2262 frames de silencio EXACTO cada 471 ms,
        // o sea 10 % de cada beat en silencio: un tartamudeo a tempo que suena
        // a chispeo. Se exige que no haya ninguna racha de silencio digital de
        // más de 10 ms (480 frames).
        for bpm in [127.4, 128.0] {
            let demo = make_demo_track(bpm, SR, 0.0);
            let mut run = 0usize;
            let mut max_run = 0usize;
            for i in 0..demo.frames() {
                if demo.samples[i * 2] == 0.0 && demo.samples[i * 2 + 1] == 0.0 {
                    run += 1;
                    max_run = max_run.max(run);
                } else {
                    run = 0;
                }
            }
            assert!(
                max_run < 480,
                "{bpm} BPM: hueco de silencio de {:.1} ms ({} frames), el demo tartamudea",
                max_run as f64 / 48.0,
                max_run
            );
        }
    }

    #[test]
    fn el_demo_tiene_imagen_estereo() {
        // Antes todo se sumaba igual en L y R (mono exacto, correlación 1). El
        // ancho tiene que venir de los hats decorrelacionados: la energía de la
        // diferencia L-R deja de ser cero.
        let demo = make_demo_track(128.0, SR, 0.0);
        let mut dif = 0.0f64;
        let mut tot = 0.0f64;
        for i in 0..demo.frames() {
            let l = demo.samples[i * 2] as f64;
            let r = demo.samples[i * 2 + 1] as f64;
            dif += (l - r) * (l - r);
            tot += l * l;
        }
        let ratio = dif / tot;
        assert!(
            ratio > 1e-3,
            "el demo debe tener ancho estéreo real, energía L-R / L = {ratio:e}"
        );
    }
}

#[cfg(test)]
mod loop_tests {
    use super::*;

    /// 120 BPM: un beat dura medio segundo y la rejilla arranca en 0.
    const BEAT: f64 = 0.5;

    fn frames(secs: f64) -> f64 {
        secs * 48_000.0
    }

    #[test]
    fn loop_de_4_beats_mide_dos_segundos() {
        let sr = 48_000;
        let (start, end) = loop_window(BEAT, 0.0, frames(1.7), sr, frames(60.0), 4.0).unwrap();
        assert!(
            (start - frames(1.5)).abs() < 1.0,
            "arranca en el beat que contiene la posición (1.5 s): {start}"
        );
        assert!(
            ((end - start) - frames(2.0)).abs() < 1.0,
            "4 beats de 0.5 s miden 2 s: {}",
            end - start
        );
    }

    #[test]
    fn loop_de_8_beats_mide_el_doble_que_el_de_4() {
        let sr = 48_000;
        let pos = frames(1.7);
        let track = frames(60.0);
        let (s4, e4) = loop_window(BEAT, 0.0, pos, sr, track, 4.0).unwrap();
        let (s8, e8) = loop_window(BEAT, 0.0, pos, sr, track, 8.0).unwrap();
        assert!((s4 - s8).abs() < 1.0, "los dos largos arrancan en el mismo beat");
        assert!(
            ((e8 - s8) - 2.0 * (e4 - s4)).abs() < 1.0,
            "8 beats miden el doble que 4: {} vs {}",
            e8 - s8,
            e4 - s4
        );
    }

    #[test]
    fn la_rejilla_desplazada_alinea_al_beat() {
        let sr = 48_000;
        // Rejilla con primer beat en 0.25 s: el loop tiene que caer en la
        // rejilla, no en el cero absoluto.
        let (start, _) = loop_window(BEAT, 0.25, frames(1.7), sr, frames(60.0), 4.0).unwrap();
        let beat_rel = (start / frames(1.0) - 0.25) / BEAT;
        assert!(
            (beat_rel - beat_rel.round()).abs() < 1e-6,
            "el borde del loop no cae en un beat de la rejilla: {beat_rel}"
        );
        assert!((start - frames(1.25)).abs() < 1.0, "beat 2 de la rejilla: {start}");
    }

    #[test]
    fn el_loop_se_recorta_al_final_del_track() {
        let sr = 48_000;
        let track = frames(3.0);
        let (start, end) = loop_window(BEAT, 0.0, frames(2.6), sr, track, 8.0).unwrap();
        assert!(end <= track - 1.0, "no pasa del último frame: {end}");
        assert!(end > start + frames(0.1), "queda algo de loop: {}", end - start);
    }

    #[test]
    fn sin_material_utilizable_no_hay_loop() {
        let sr = 48_000;
        assert!(loop_window(0.0, 0.0, frames(1.0), sr, frames(60.0), 4.0).is_none());
        assert!(loop_window(BEAT, 0.0, frames(1.0), sr, frames(60.0), 0.0).is_none());
    }
}

#[cfg(test)]
mod loop_shadow_tests {
    use super::*;

    const LANE: f64 = 1000.0;

    #[test]
    fn el_loop_centrado_ocupa_el_medio_del_carril() {
        // 200 frames de loop, 1 px por frame, playhead justo en el centro del
        // loop: la sombra arranca 100 px a la izquierda del centro.
        let (x, w) = loop_shadow(1_900.0, 2_100.0, 2_000.0, 1.0, LANE).unwrap();
        assert!((x - 400.0).abs() < 1e-3, "x: {x}");
        assert!((w - 200.0).abs() < 1e-3, "ancho: {w}");
    }

    #[test]
    fn la_sombra_se_corre_con_el_deck() {
        // Al avanzar el deck, el mismo tramo se va hacia la izquierda.
        let (x_antes, _) = loop_shadow(1_900.0, 2_100.0, 2_000.0, 1.0, LANE).unwrap();
        let (x_despues, w) = loop_shadow(1_900.0, 2_100.0, 2_100.0, 1.0, LANE).unwrap();
        assert!(x_despues < x_antes, "se corre a la izquierda: {x_antes} -> {x_despues}");
        assert!((w - 200.0).abs() < 1e-3, "el ancho no cambia: {w}");
    }

    #[test]
    fn el_recorte_al_borde_no_inventa_ancho() {
        // Ya casi salió por la izquierda: lo que queda se recorta al carril.
        let (x, w) = loop_shadow(1_900.0, 2_100.0, 2_500.0, 1.0, LANE).unwrap();
        assert!(x >= 0.0, "x nunca negativo: {x}");
        assert!((w - 100.0).abs() < 1e-3, "sólo queda la cola visible: {w}");
    }

    #[test]
    fn un_loop_fuera_del_carril_no_dibuja_sombra() {
        assert!(loop_shadow(1_900.0, 2_100.0, 4_000.0, 1.0, LANE).is_none());
        assert!(loop_shadow(1_900.0, 2_100.0, 2_000.0, 0.0, LANE).is_none());
        assert!(loop_shadow(2_100.0, 1_900.0, 2_000.0, 1.0, LANE).is_none());
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;

    const SR: f64 = 48_000.0;

    /// La ley que hace que al maximizar no se rompa nada: un tramo de audio
    /// ocupa siempre `ancho_del_carril * segundos / zoom` píxeles, sin importar
    /// el tamaño de la ventana ni la resolución del bitmap.
    #[test]
    fn un_segundo_ocupa_lo_mismo_en_cualquier_tamano() {
        for lane_w in [1180.0, 1900.0, 2560.0] {
            let (px_por_frame, _) = wave_scale(8.0, SR, lane_w);
            let segundos_por_px = 1.0 / (SR * px_por_frame);
            let px_de_2s = 2.0 / segundos_por_px;
            assert!(
                (px_de_2s - lane_w * 2.0 / 8.0).abs() < 0.5,
                "con zoom 8 s, 2 s deben medir un cuarto del carril ({lane_w} px): {px_de_2s}"
            );
        }
    }

    /// El ancho de la sombra tiene que seguir a la escala del carril: si se
    /// dibujara con la constante de diseño, al maximizar marcaría otro tramo.
    #[test]
    fn la_sombra_usa_el_ancho_real_del_carril() {
        let (px_por_frame, _) = wave_scale(8.0, SR, 2560.0);
        let dos_seg = 2.0 * SR;
        let (x, w) = loop_shadow(10.0 * SR, 12.0 * SR, 11.0 * SR, px_por_frame, 2560.0).unwrap();
        assert!((w - 640.0).abs() < 0.5, "2 s de 8 son 640 px de 2560: {w}");
        assert!((x - (1280.0 - 320.0)).abs() < 0.5, "centrado en el playhead: {x}");
        assert!(dos_seg > 0.0);
    }

    /// El bitmap acompaña al carril con la misma densidad: si no, maximizar
    /// estira un bitmap chico y la onda se ve borrosa.
    #[test]
    fn el_bitmap_conserva_la_densidad_al_maximizar() {
        let (w_chico, h_chico) = strip_size(1180.0, 216.0);
        let (w_grande, h_grande) = strip_size(2560.0, 700.0);
        assert_eq!((w_chico, h_chico), (590, 108));
        assert_eq!((w_grande, h_grande), (1280, 350));
    }

    #[test]
    fn el_bitmap_tiene_piso_para_ventanas_angostas() {
        let (w, h) = strip_size(200.0, 30.0);
        assert_eq!((w, h), (LANE_W / WAVE_DIV, LANE_MIN_PY as u32));
    }
}
