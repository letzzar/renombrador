# Renombrador

**English** | [Español](#español)

---

A native Windows desktop app built in Rust (egui) that renames movie and TV show video files using the [TMDb](https://www.themoviedb.org/) API, with multi-language title support.

## Features

- Scans a folder for video files (`.mkv`, `.mp4`, `.avi`)
- Searches TMDb for the official title using smart filename parsing (strips dots, brackets, quality tags)
- Renames to clean format: `Movie Title (Year).mkv` / `Show S01E02 - Episode Name.mkv`
- Native GUI with [egui](https://github.com/emilk/egui) — single `.exe`, no installer needed
- Multi-language results: Spanish (ES/MX), English (US/GB), French, and more
- TMDb API key saved persistently in `config.json`
- Copy result to clipboard

## Requirements

- Windows 10+
- A free [TMDb API key](https://www.themoviedb.org/settings/api)

## Build

```bash
cargo build --release
```

The executable is placed in `target/release/renombrador.exe`.

## Usage

1. Launch `renombrador.exe`
2. Enter your TMDb API key on first launch
3. Click **Select folder** and choose the directory with your video files
4. The app proposes renames — confirm to apply

---

## Español

App de escritorio nativa para Windows construida en Rust (egui) que renombra archivos de películas y series usando la API de [TMDb](https://www.themoviedb.org/), con soporte multiidioma.

## Características

- Escanea una carpeta en busca de archivos de vídeo (`.mkv`, `.mp4`, `.avi`)
- Busca en TMDb el título oficial mediante análisis inteligente del nombre de archivo (elimina puntos, corchetes, etiquetas de calidad)
- Renombra al formato limpio: `Película (Año).mkv` / `Serie S01E02 - Nombre episodio.mkv`
- Interfaz nativa con [egui](https://github.com/emilk/egui) — un solo `.exe`, sin instalador
- Resultados multiidioma: español (ES/MX), inglés (US/GB), francés y más
- Clave API de TMDb guardada persistentemente en `config.json`
- Copia el resultado al portapapeles

## Requisitos

- Windows 10+
- Una [clave API de TMDb](https://www.themoviedb.org/settings/api) gratuita

## Compilar

```bash
cargo build --release
```

El ejecutable queda en `target/release/renombrador.exe`.

## Uso

1. Lanza `renombrador.exe`
2. Introduce tu clave API de TMDb en el primer arranque
3. Haz clic en **Seleccionar carpeta** y elige el directorio con tus archivos de vídeo
4. La app propone los cambios de nombre — confirma para aplicar

## Licencia

MIT © 2026 letzzar
