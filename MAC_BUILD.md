# Handoff: compilar versión macOS

Notas para la próxima sesión de Claude en el Mac. El usuario quiere
que yo (Claude) haga la compilación allí.

> **⚠️ Actualización (servicio Docker):** el proyecto ahora compila dos binarios.
> La **GUI** está detrás de la feature `gui` y se llama `renombrador-gui`. Para el
> Mac usa **`cargo build --release --features gui --bin renombrador-gui`** y el
> binario queda en `target/release/renombrador-gui`. Lo mismo para `cargo bundle`:
> `cargo bundle --release --features gui --bin renombrador-gui`. El otro binario,
> `renombrador-daemon`, es el servicio Linux y no aplica al empaquetado de macOS.

## Estado actual

Commit relevante: `19603eb` — *Fix episode collision, portable config and clipboard, window icon*.

Trabajo ya hecho en Windows y verificado (`cargo build --release` OK):

- Bug de colisión de episodios corregido: auto‑detección serie/película por
  archivo según marcadores `SxxEyy` / `NxM`, más guard `renombrar_si_seguro`
  que rechaza sobreescribir un destino existente.
- `config.json` migrado a la carpeta estándar del sistema. En macOS:
  `~/Library/Application Support/renombrador/config.json`. Migración suave
  automática desde un `config.json` antiguo en el cwd.
- Icono de ventana cargado en runtime desde `logo_app.ico`
  (`egui::ViewportBuilder::with_icon`).
- Portabilidad:
  - `winres` movido a `[target.'cfg(windows)'.build-dependencies]` y
    gateado con `#[cfg(windows)]` en `build.rs`.
  - `clipboard-win` reemplazado por `arboard` (multiplataforma).

## Lo que toca en el Mac

```bash
git pull
cargo build --release
./target/release/renombrador
```

Si es la primera vez en el Mac:

```bash
xcode-select --install
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

## Preguntas abiertas (preguntar al usuario al arrancar)

1. **Arquitectura**: ¿build nativa solo (la arch del Mac actual) o **universal**
   (Intel + Apple Silicon vía `lipo`)? Si universal:
   ```bash
   rustup target add x86_64-apple-darwin aarch64-apple-darwin
   cargo build --release --target x86_64-apple-darwin
   cargo build --release --target aarch64-apple-darwin
   lipo -create -output renombrador \
       target/x86_64-apple-darwin/release/renombrador \
       target/aarch64-apple-darwin/release/renombrador
   ```
2. **Empaquetado**: ¿binario suelto, **.app bundle** (necesita `Info.plist` +
   `.icns` convertido desde `logo_app.ico`), o **.dmg**?
   - Para `.app`: `cargo install cargo-bundle` y añadir
     `[package.metadata.bundle]` en `Cargo.toml`.
   - Para `.icns`: `iconutil -c icns logo_app.iconset/` tras generar el
     `.iconset` desde el PNG (o `sips` desde el `.ico`).
3. **Firma / notarización**: ¿solo para uso personal o distribuir a otros? La
   notarización de Apple requiere cuenta de desarrollador.

## Recordatorios

- No hace falta tocar `clipboard-win`/`winres` — ya están gateados.
- Si `cargo build` se queja por `arboard` en Linux/Mac por backends de
  Wayland/X11 (no debería en macOS), revisar features.
- `MAC_BUILD.md` se puede borrar tras terminar el setup.
