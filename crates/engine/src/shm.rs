//! Controles compartidos entre la UI y el proceso de audio.
//!
//! Por qué existe: el hilo de audio no puede convivir con el render de la
//! ventana en el mismo proceso. Medido en este equipo, con la UI andando a
//! 30 Hz se pierde 1-2 % de los frames (el cliente entrega bloques tarde y
//! PipeWire los descarta), y sin la UI, 99.98 %. Se escucha como chispeo.
//!
//! La solución es que el motor corra en un proceso aparte y que los dos se
//! comuniquen por estos atómicos en un archivo mapeado: el mismo `Controls`
//! que ya usaba el callback, más una cola mínima para pedir cargas de tracks.

use crate::{Controls, DECK_COUNT};
use std::fs::OpenOptions;
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Largo máximo de una ruta de archivo.
pub const PATH_MAX: usize = 512;

/// Pedido de carga de un deck: la UI escribe la ruta y sube `gen`; el motor la
/// lee y copia `gen` en `ack`. Una sola escritura y una sola lectura.
#[repr(C)]
pub struct LoadSlot {
    pub gen: AtomicU64,
    pub ack: AtomicU64,
    pub len: AtomicU32,
    pub path: [u8; PATH_MAX],
}

impl Default for LoadSlot {
    fn default() -> Self {
        Self {
            gen: AtomicU64::new(0),
            ack: AtomicU64::new(0),
            len: AtomicU32::new(0),
            path: [0; PATH_MAX],
        }
    }
}

#[repr(C)]
pub struct Shared {
    pub controls: Controls,
    /// Los publica el proceso de audio cuando abre el dispositivo; la UI los
    /// necesita para la geometría de las ondas.
    pub sample_rate: AtomicU32,
    pub buffer_frames: AtomicU32,
    pub loads: [LoadSlot; DECK_COUNT],
}

/// Archivo mapeado con el bloque compartido. Vive lo que vive el proceso.
pub struct SharedMapping {
    ptr: *mut Shared,
    size: usize,
}

unsafe impl Send for SharedMapping {}
unsafe impl Sync for SharedMapping {}

impl SharedMapping {
    pub fn block(&self) -> &Shared {
        unsafe { &*self.ptr }
    }

    /// Crea (o recrea) el bloque. Sólo lo hace el proceso de UI.
    pub fn create(path: &Path) -> io::Result<Self> {
        Self::map(path, true)
    }

    /// Se engancha a un bloque existente. Sólo lo hace el proceso de audio.
    pub fn attach(path: &Path) -> io::Result<Self> {
        Self::map(path, false)
    }

    fn map(path: &Path, create: bool) -> io::Result<Self> {
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(create)
            .open(path)?;
        let size = std::mem::size_of::<Shared>();
        if (f.metadata()?.len() as usize) < size {
            f.set_len(size as u64)?;
        }
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                f.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        if create {
            // Arranque limpio con los defaults REALES: poner todo en cero deja
            // rate = 0 (la posición avanza a media velocidad) y gain = 0 (silencio).
            let init = Shared {
                controls: Controls::default(),
                sample_rate: AtomicU32::new(0),
                buffer_frames: AtomicU32::new(0),
                loads: std::array::from_fn(|_| LoadSlot::default()),
            };
            unsafe { std::ptr::write(ptr as *mut Shared, init) };
        }
        Ok(Self {
            ptr: ptr as *mut Shared,
            size,
        })
    }
}

impl Drop for SharedMapping {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr as *mut libc::c_void, self.size);
        }
    }
}

/// La UI pide que el motor cargue `path` en `deck`.
pub fn request_load(shared: &Shared, deck: usize, path: &str) {
    if deck >= DECK_COUNT {
        return;
    }
    let slot = &shared.loads[deck];
    let bytes = path.as_bytes();
    let n = bytes.len().min(PATH_MAX - 1);
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), slot.path.as_ptr() as *mut u8, n);
    }
    slot.len.store(n as u32, Ordering::Relaxed);
    slot.gen.fetch_add(1, Ordering::Release);
}

/// El motor consulta si hay un pedido pendiente. Devuelve la ruta y marca el
/// pedido como atendido.
pub fn take_load(shared: &Shared, deck: usize) -> Option<String> {
    if deck >= DECK_COUNT {
        return None;
    }
    let slot = &shared.loads[deck];
    let gen = slot.gen.load(Ordering::Acquire);
    if gen == slot.ack.load(Ordering::Relaxed) {
        return None;
    }
    let n = (slot.len.load(Ordering::Relaxed) as usize).min(PATH_MAX - 1);
    let bytes = unsafe { std::slice::from_raw_parts(slot.path.as_ptr(), n) };
    let path = String::from_utf8_lossy(bytes).into_owned();
    slot.ack.store(gen, Ordering::Release);
    Some(path)
}
