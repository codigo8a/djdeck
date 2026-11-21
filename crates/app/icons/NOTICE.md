# Iconos de terceros

## Material Symbols (Google LLC)

Los iconos del transporte de cada deck se embeben como `Path` de Slint con los
`commands` tomados de Material Symbols, variante *rounded*, estilo *filled*.

| Icono en la UI | Nombre en Material Symbols |
|---|---|
| play / pause | `play_arrow` / `pause` |
| CUE | `skip_previous` |
| SYNC | `sync` |
| CARGAR | `folder` |
| LOOP | `repeat` (el largo en beats va al lado, como número) |

Origen: <https://github.com/google/material-design-icons>
(`src/<categoría>/<nombre>/materialiconsround/24px.svg`).

Copyright Google LLC. Licenciado bajo la Licencia Apache, Versión 2.0.
Copia de la licencia: `LICENSE-APACHE-2.0.txt` en este mismo directorio.
Texto de referencia: <https://www.apache.org/licenses/LICENSE-2.0>

### Cambios respecto del original

- Se extrajo únicamente el atributo `d` de cada path.
- Se descartó el path transparente de relleno (`fill="none"`) que traen algunos
  SVG para fijar el viewBox.
- Se quitó el viewBox `0 0 24 24` del archivo y se declara en Slint con
  `viewbox-width` / `viewbox-height`; el tamaño en pantalla lo fija el botón
  (18 px), no el archivo.
- El color no viene del SVG: cada icono usa el `ink` del botón (estado normal,
  hover, presionado o activo), que a su vez sale del acento del deck.
- `folder_open` se reemplazó por `folder`: la variante abierta tiene un hueco
  (subpath interno) y el renderer software de Slint lo dibuja macizo.
