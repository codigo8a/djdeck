# djdeck

Deck DJ nativo para Linux, en Rust. MVP: **dos platos, crossfader, gain y sync por beatgrid**.
Sin navegador, sin Electron, sin Wine: el motor habla directo con ALSA/PipeWire.

```
djdeck "lado-a.mp3" "lado-b.flac"   # carga tus archivos
djdeck                              # arranca con dos demos sintéticos (127.4 vs 128.0 BPM)
djdeck --selftest                   # verificación sin UI ni placa de sonido
```

## Qué hace hoy

- Dos decks independientes: play/pausa, cue al primer beat, tiempo transcurrido y total.
- Ondas **apiladas** (A arriba, B abajo) sobre el mismo eje de tiempo, con zoom de 1 a 64 s,
  rejilla de beats (cada 4 más marcada) y playhead fijo al centro: si el sync está bien, los
  kicks de los dos temas caen en la misma vertical.
- Pitch fader ±8 % (modo vinilo: cambia tempo *y* tono — ver deudas). **Doble click en el fader
  vuelve a 0 % en el centro**; si ese deck estaba en SYNC, el doble click apaga su SYNC, porque el
  lazo de sync reescribe el rate a 30 Hz y el 0 % no duraría un frame. Con SYNC activo el fader y el
  texto se mueven con el lazo: muestra el % real que aplicó el sync y el BPM destino (`126.2 → 131.9`).
- `LOOP 8`: engancha 8 beats desde el beat que contiene la posición actual y envuelve con un fade de
  2 ms en el borde (medido: el salto máximo entre muestras en el loop queda bajo 0.05, sin click).
- EQ de 3 bandas **real** por deck (low shelf 200 Hz, campana 1 kHz, high shelf 4 kHz, ±12 dB), con
  bypass exacto cuando las tres están en 0 dB.
- Gain por canal + crossfader equal-power (−3 dB en el centro) + limitador suave.
- Sync: iguala tempo (relación de BPM, recortada a ±8 %) y corrige fase contra el otro deck
  con un offset de rate de hasta ±3 %, con deadband de 0.004 beat para no vibrar.
- Análisis al cargar: BPM y fase de la rejilla por autocorrelación de la envolvente de onsets.
- Salida de audio por `cpal` a 48 kHz, bloques de 256 frames (5.3 ms).

## Qué no hace todavía (deudas explícitas)

| Falta | Consecuencia |
|---|---|
| Keylock / master tempo | mover el pitch cambia el tono. A ±2 % no se nota, a ±8 % sí. |
| Cue / prelisten | los dos decks suenan por la misma salida: **no sirve para tocar en vivo todavía**. |
| Filtro (LPF/HPF) | el EQ es de 3 bandas; el filtro por canal todavía no está. |
| Platos / jog wheels | decisión explícita: no se usan; el transporte es por botones y el pitch por fader. |
| Hotcues y loops manuales | hay loop de 8 beats al vuelo; faltan loop in/out a mano, 1/2/4/16 beats y hotcues. |
| Biblioteca / base de datos | los tracks se pasan por línea de comandos. |
| Detección de compás (downbeat) | el sync alinea a nivel de beat, no de compás: dos temas pueden quedar alineados a contratiempo. |
| Stems | no hay separación de voces/instrumental. En esta máquina demucs.cpp mide 0.094× tiempo real: es trabajo de importación, no de reproducción. |
| Decodificación en streaming | el MVP decodifica el track entero a RAM (un tema de 4 min ≈ 55 MB). |

## Stack y por qué

| Rol | Crate | Motivo |
|---|---|---|
| Salida de audio | `cpal` 0.15 | ALSA nativo en Linux; PipeWire a través de su plugin ALSA. |
| Decodificación | `symphonia` 0.5 | mp3/aac/flac/wav/aiff/isomp4 en Rust puro, sin ffmpeg del sistema. |
| UI | `slint` 1.19 (renderer software) | Sin GPU dedicada en el equipo objetivo: llvmpipe arrastra la waveform. |
| Remuestreo | propio (sinc con ventana de Kaiser, polifásico) | Un solo punto de conversión al SR del dispositivo, testeable, sin dependencia extra. Filtra las imágenes espectrales: una interpolación polinómica las deja caer en la banda audible. |
| Análisis | propio | Autocorrelación de envolvente: determinística y verificable con click-tracks. |
| Hilos | atómicos + cola con `try_lock` | El callback de audio no decodifica, no reserva memoria y no espera un lock por un fader. |

## Arquitectura

```
crates/
  engine/    decode → remuestreo → decks → mezcla → salida (sin UI, testeable offline)
  analysis/  BPM + fase de rejilla, picos de waveform
  app/       UI Slint + lazo de control (20 Hz) + demos sintéticos
```

**El audio corre en su propio proceso.** La UI lanza `djdeck --audio-engine`, que abre el
dispositivo y mezcla; los dos se hablan por `engine::shm` (un archivo mapeado en
`$XDG_RUNTIME_DIR/djdeck-shm` con los mismos atómicos de `Controls` y una cola de cargas por deck).
Motivo medido: si la UI y el hilo de audio comparten proceso, el repintado de la ventana hace que el
cliente entregue bloques tarde y PipeWire los descarta — 98 % de frames renderizados y chispeo
audible; con el motor aparte, 99.9-99.98 %.

Reglas que no se rompen:

1. El callback de audio sólo lee controles atómicos y hace sumas y multiplicaciones.
2. La posición de un deck se cuenta en frames al SR del dispositivo, nunca en frames del archivo.
3. Todo lo que suena se puede renderizar offline, y por eso se testea sin placa de sonido.

## Verificación

```
cargo test -p engine -p analysis    # render offline determinístico + detección de BPM
cargo run --release -p app -- --selftest
cargo run --release -p app -- --autoplay=30   # punta a punta con la placa real
```

Los tests cubren: ley del crossfader (−3.01 dB en el centro), transparencia del limitador bajo 0.8,
suma analítica de dos decks idénticos (±1e-5), separación de canales, mute por gain, publicación de
posiciones y longitudes, seek, conservación de frecuencia en el remuestreo 44.1→48 kHz, y convergencia
del sync a <0.02 beat de error tras 5 s en una mezcla de 60 s, y el EQ (identidad exacta en 0 dB,
shelf de graves +12 dB ≈ 4x, shelf de agudos −12 dB, campana de 1 kHz que no toca los 60 Hz).
**32 tests, todos en verde.**

`cargo test -p engine --test audio_quality` mide **calidad**, no sólo comportamiento: analiza el
espectro de lo que el motor renderiza (ventana Blackman-Harris, espurios in-band contra la
fundamental) y fija cotas superiores de distorsión. Un arreglo que mejore la calidad sigue pasando;
una regresión falla. Lo medido hoy, con los puntos flojos declarados como deuda:

| Medición | Hoy |
|---|---|
| Pitch en 0 % | **bit-exacto** (desvío 0) |
| EQ en 0 dB | **bit-exacto**, y el EQ activo cumple escalado y superposición (<1e-4) |
| Limiter bajo 0.8 de entrada | transparente (−103 dB) |
| Limiter a 1.35 de entrada | −18.9 dB con armónico en 3 kHz *(deuda: es un tanh, sin lookahead)* |
| Remuestreo 44.1→48, 1 kHz | −89 dB |
| Remuestreo 44.1→48, 10 kHz / 15 kHz | **−98 dB / −94 dB** (sinc polifásico, 48 taps, ventana de Kaiser) |
| Pitch ±2…8 %, 1 kHz | −64 dB |
| Pitch +8 %, 10 kHz / 15 kHz | **−22 dB / −13.5 dB** *(deuda: `frame_at` interpola lineal; un cúbico lo bajaría ~40 dB)* |
| EQ mid +12 dB | **−12.9 dB** *(deuda: el EQ va antes del limiter y no compensa headroom)* |
| Dos decks al centro, master 0.9 | −58 dB contra la suma lineal ideal |

Los dos tests de `cargo test -p app` fijan el contenido del demo sintético, que es lo que suena
cuando no se pasan archivos: el hat tiene que estar limitado a su banda (6–12 kHz) y el tema tiene
que tener ancho estéreo real. Antes el hat era ruido blanco crudo —energía plana hasta Nyquist—
medido a 0.5 dB de diferencia entre 2 kHz y 9 kHz; con el pasa-banda mide 16 dB.

### Medido en el equipo objetivo (Celeron N5095, Debian 13, PipeWire)

| Prueba | Resultado |
|---|---|
| `cargo test` | 19/19 en verde (12 render + 4 EQ + 3 analysis) |
| Sync offline, 30 s a 127.4 vs 128.0 BPM | peor error de fase tras 5 s: **0.0001 beats**; sin saturar (pico 0.946) |
| Sync con dispositivo real, 30 s | peor error de fase: **0.001 beats**; 0 xruns; 100.0 % del tiempo real renderizado |
| Buffer 1024 frames (21.3 ms) | 99.7 % del tiempo real, callback de 1024/256 frames, 0 xruns |
| Buffer 256 frames (5.3 ms) | **84 % del tiempo real**: pipewire-alsa entra en underrun sostenido y el reloj de audio se atrasa. Por eso el default es 1024 |
| Quantum 256 vs 1024 en el grafo de PipeWire | Con 256 el cliente tiene **5.33 ms** de margen por ciclo y un repintado de la UI (4–7 ms) lo hace llegar tarde: **98 % de los frames** renderizados, un chasquido cada ~300 ms. La app ahora pide `PIPEWIRE_LATENCY = buffer/SR` al abrir el dispositivo (1024/48000) y el nodo negocia 21.3 ms en vez de 5.33 |
| Detección de BPM, material real | "Can't Get You Out Of My Head" → **126.2** (real 126); remix de Tiësto → 131.9 |
| UI con las dos ondas a 30 Hz (binario debug) | 35 % de un núcleo, 217 MB RSS, 0 underruns en 100 s |
| Sync de 126.2 contra 131.9 BPM | offset aplicado **+4.5 %**, rejillas alineadas en pantalla, `126.2 → 131.9` |

`DJDECK_FRAMES` y `DJDECK_RATE` permiten forzar buffer y frecuencia; `DJDECK_DEVICE` fuerza un
dispositivo por subcadena del nombre; `DJDECK_FORMAT=s16|s32|f32` fuerza el formato de muestra
(default `f32`).

**Importante**: la app pide una sola instancia. Dos djdeck a la vez suman el mismo audio con
offsets distintos, y eso no suena a "dos canciones": suena a **chispeo** (peine de fase). Si ves
el mensaje `ya hay una instancia de djdeck corriendo`, no es un error del motor.

**Si escuchás por escritorio remoto** (RDP/VNC), el audio y el video de la ventana comparten el
mismo enlace: `gnome-remote-desktop` captura el monitor del sink y lo codifica, y cada actualización
de la interfaz compite con el stream de audio. El síntoma es un chispeo que aparece **sólo mientras
hay sonido**, no depende del volumen, empeora cuanto más se mueve la ventana y **no existe en
ninguna captura digital del motor** (medido: render idéntico a la referencia de soxr a −108 dB). No
es la app. Por eso el refresco de la UI por defecto es **66 ms (15 Hz)**: medido por RDP, a 30 Hz se
escucha chispeo, a 15 Hz limpio, a 1 Hz limpio. Con auriculares en la máquina local,
`DJDECK_UI_MS=33` devuelve los 30 Hz completos sin penalidad.

Confirmado con la prueba definitiva: **con la ventana en segundo plano el audio sale limpio** (no hay
video que codificar). Para escuchar por escritorio remoto con la UI a la vista está
`DJDECK_REMOTO=1`, que baja el refresco a 1 Hz: la ventana queda casi estática y el audio deja de
competir con el video.

### Cómo arrancar según dónde escuchás

| Escenario | Comando |
|---|---|
| Pantalla física en la máquina (lo ideal) | `./target/release/app` + `DJDECK_UI_MS=33` para los 30 Hz completos |
| Escritorio remoto con cliente conectado | `DJDECK_REMOTO=1` (1 Hz) o `DJDECK_UI_MS=200` |

Con pantalla física, además conviene darle prioridad realtime al hilo de audio; hoy no la tiene porque
no hay límite de `rtprio` para el usuario:

```
sudo setcap cap_sys_nice+ep target/release/app   # se pierde en cada recompilación
```

Con eso el hilo de audio del motor corre en `SCHED_FIFO` (verificable con `ps -L -o rtprio -p <pid del
motor>`) y deja de depender de que el resto del sistema le ceda CPU.

Para diagnosticar el camino de audio hay dos variables más: `DJDECK_DIAG=<segundos>` arranca la UI,
reproduce los dos decks y reporta cada 5 s qué porcentaje de los frames se renderizaron, los xruns,
las posiciones, los picos y el costo máximo del tick de UI; `DJDECK_UI_MS=<ms>` cambia el período de
refresco (default 33 ms) para medir cuánto come el render de las ondas. El `%` tiene que quedar en
99.9 o más: cada décima que falta son bloques descartados, o sea chasquidos.

## Roadmap

1. Keylock (`signalsmith-stretch`) y pitch range ±16 %.
2. Cue a segunda placa de audio.
3. Filtro por canal (LPF/HPF) y EQ con knobs en vez de sliders.
4. Downbeat/phrase (beat-this con golden set) — alinear a compás, no sólo a beat.
5. Hotcues, loops manuales con quantize, biblioteca SQLite.
6. Stems precalculados al importar (motor de 4 stems por deck).
7. MIDI learn, grabación del set, empaquetado `.deb`/AppImage.

## Licencia

MIT.
