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

## Prerequisites

| Requirement | Notes |
|---|---|
| **Rust toolchain** | Install via [rustup.rs](https://rustup.rs) — download and run `rustup-init.exe` |
| **MSVC Build Tools** | Download [Visual Studio Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/) → select **"Desktop development with C++"**. Required for the Rust Windows toolchain. |
| **TMDb API key** | Free — see below |

### Getting a TMDb API key (free)

1. Create a free account at [themoviedb.org](https://www.themoviedb.org/signup)
2. Go to **Settings → API** → click **Create** → choose **Developer**
3. Fill in the form (app name: anything, e.g. "Renombrador"; URL: your website or `http://localhost`)
4. Copy the **API Key (v3 auth)**

## Build

```bash
git clone https://github.com/letzzar/renombrador.git
cd renombrador

cargo build --release
```

The executable is placed in `target\release\renombrador.exe`.

> **Tip:** On first build, Cargo downloads and compiles all dependencies (~2–5 min). Subsequent builds are much faster.

## Usage

1. Launch `renombrador.exe`
2. Enter your TMDb API key on first launch (saved automatically to `config.json`)
3. Click **Select folder** and choose the directory with your video files
4. Select the desired language for titles
5. Review the proposed renames — click **Apply** to rename

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

## Requisitos previos

| Requisito | Notas |
|---|---|
| **Rust** | Instala desde [rustup.rs](https://rustup.rs) — descarga y ejecuta `rustup-init.exe` |
| **MSVC Build Tools** | Descarga [Visual Studio Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/) → selecciona **"Desarrollo de escritorio con C++"**. Necesario para el toolchain Rust en Windows. |
| **Clave API de TMDb** | Gratuita — ver instrucciones abajo |

### Cómo obtener una clave API de TMDb (gratis)

1. Crea una cuenta gratuita en [themoviedb.org](https://www.themoviedb.org/signup)
2. Ve a **Configuración → API** → haz clic en **Crear** → elige **Desarrollador**
3. Rellena el formulario (nombre de la app: lo que quieras, p. ej. "Renombrador"; URL: tu web o `http://localhost`)
4. Copia la **Clave API (autenticación v3)**

## Compilar

```bash
git clone https://github.com/letzzar/renombrador.git
cd renombrador

cargo build --release
```

El ejecutable queda en `target\release\renombrador.exe`.

> **Consejo:** En la primera compilación, Cargo descarga y compila todas las dependencias (~2–5 min). Las siguientes compilaciones son mucho más rápidas.

## Uso

1. Lanza `renombrador.exe`
2. Introduce tu clave API de TMDb en el primer arranque (se guarda automáticamente en `config.json`)
3. Haz clic en **Seleccionar carpeta** y elige el directorio con tus archivos de vídeo
4. Selecciona el idioma deseado para los títulos
5. Revisa los cambios propuestos — haz clic en **Aplicar** para renombrar

## Licencia

MIT © 2026 letzzar
