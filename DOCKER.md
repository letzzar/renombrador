# Renombrador TMDb — Docker service

**English** | [Español](#español)

Headless (no-GUI) version of the desktop app. It runs as a Docker service,
**watches your downloads folder**, and as soon as a new video appears it
identifies it on TMDb, **renames** it with the official title and **moves** it to
your movies/series library using the layout that Plex/Jellyfin/Emby expect.

It reuses the exact same parsing and search logic as the desktop app
(progressive multi-language search + similarity scoring), but without human
interaction.

---

## What it does, step by step

1. Every `POLL_INTERVAL` seconds it scans the downloads folder **recursively**
   for `.mkv`, `.mp4`, `.avi` files.
2. It waits until each file is **stable** (its size hasn't changed for
   `STABLE_SECS`) so it never touches a half-finished download.
3. It cleans the filename of release tags (resolution, source, codec, group…)
   and extracts the title, the release **year** and the season/episode if
   present. The text *after* the episode code is considered too: in names like
   `Star Trek 1x01 Star Trek Strange New Worlds (2022)` the real show name
   lives there, and searching only for `Star Trek` matched the 1966 series
   perfectly. A normal episode title is still ignored.
4. It searches TMDb for the official title in your language, passing the year
   as a filter instead of embedding it in the query. If it finds a confident
   match — score ≥ `MATCH_THRESHOLD` **and** no near-tie between candidates:
   - **Movie** → `Movies/Title (Year)/Title (Year).mkv`
     (and, if it belongs to a saga, inside `TMDb Collection/…`).
   - **Show** → `Series/Show (Year)/Season 01/Show 1x01 Episode.mkv`.
5. If it is **not** confident, it applies the `ON_UNCERTAIN` policy (by default,
   move to `downloads/_revisar` so you can check it by hand). A near-tie counts
   as not confident even at score 1.00: when several candidates land within
   0.02 of each other the real decision would be made by popularity, which is
   not good enough to rename unattended. Only firm matches are cached.
6. It caches the resolved series id in `/config/cache.json`, so the next
   episodes are renamed instantly without searching again. Episode names are
   fetched **one season at a time** and kept in memory, so a full-season batch
   costs ~2 TMDb calls instead of 2 per episode.
7. If TMDb is **unreachable** (network down, timeout, server error), files are
   **left untouched** and retried with exponential backoff (2·, 4·, 8·… the
   poll interval, capped at 15 min). A network outage never sends files to
   quarantine.

> Moving is safe: it **never overwrites** a different existing file, and it works
> even when downloads and movies live on **different volumes** (it copies to a
> temporary `.part` file and renames at the end, so a crash mid-copy never
> leaves a half-written video at the destination).

> **Tip — first run:** set `DRY_RUN=true` and watch the log. The service prints
> the exact destination it *would* use for every file without moving anything.
> When you are happy with the result, set it back to `false`.

---

## Two ways to run it

- **A) Prebuilt image (recommended):** pull a ready image from Docker Hub / GHCR.
  Nothing is compiled on your machine/NAS.
- **B) Build it yourself:** Docker compiles the Rust binary inside the container.

### Get a TMDb API key (free)

1. Create an account at <https://www.themoviedb.org/signup>.
2. **Settings → API** → *Create* → *Developer*.
3. Copy the **API Key (v3 auth)** into `TMDB_API_KEY`.

### A) Run the prebuilt image

```bash
cp .env.example .env          # edit TMDB_API_KEY and the host paths
docker compose -f docker-compose.pull.yml up -d
docker compose -f docker-compose.pull.yml logs -f
```

(Edit the `image:` line in `docker-compose.pull.yml` to your Docker Hub image,
e.g. `youruser/renombrador:latest`, or use `ghcr.io/letzzar/renombrador:latest`.)

### B) Build it yourself

```bash
cp .env.example .env          # edit TMDB_API_KEY and the host paths
docker compose up -d --build
docker compose logs -f
```

Helper scripts: `./scripts/start.sh`, `./scripts/logs.sh`, `./scripts/stop.sh`.

---

## Folders / mounts

| Inside container | Host variable      | Purpose                         |
|------------------|--------------------|---------------------------------|
| `/descargas`     | `DOWNLOADS_DIR`    | Watched folder (input).         |
| `/peliculas`     | `MOVIES_HOST_DIR`  | Movies destination.             |
| `/series`        | `SERIES_HOST_DIR`  | Series destination.             |
| `/config`        | `CONFIG_DIR`       | Persistent cache (`cache.json`).|

> For instant moves, keep downloads/movies/series on the **same** filesystem. If
> they live on different disks it still works (copy + delete), just slower.

---

## Environment variables

| Variable           | Default                | Description |
|--------------------|------------------------|-------------|
| `TMDB_API_KEY`     | — (**required**)       | TMDb v3 API key. |
| `TMDB_LANGUAGE`    | `es-ES`                | Title language: `es-ES`, `es-MX`, `en-US`, `en-GB`, `fr-FR`, `de-DE`, `it-IT`, `pt-BR`. |
| `NESTED`           | `true`                 | `true` = nested Plex/Jellyfin layout; `false` = flat. |
| `YEAR_FORMAT`      | `parens`               | `parens` = `Title (2021)` (Plex standard); `brackets` = `Title [2021]`. |
| `USE_COLLECTIONS`  | `true`                 | Group sagas in a folder named after the TMDb collection (nested only). |
| `EPISODE_FORMAT`   | `1x05`                 | `1x05` or `S01E05`. |
| `FORCE_MODE`       | `auto`                 | `auto` (detect by name), `series` or `movies`. |
| `ON_UNCERTAIN`     | `revisar`              | Doubtful match: `revisar` (quarantine), `dejar` (leave), `forzar` (use best anyway). |
| `MATCH_THRESHOLD`  | `0.85`                 | Similarity threshold [0..1] to rename without doubt. |
| `DRY_RUN`          | `false`                | `true` = log what *would* be done (calculated destinations) without moving anything. |
| `POLL_INTERVAL`    | `30`                   | Seconds between folder scans. |
| `STABLE_SECS`      | `60`                   | Seconds a file must be unchanged before processing. |
| `MIN_FILE_MB`      | `50`                   | Minimum size (MB); skips *samples*. |
| `CLEAN_EMPTY_DIRS` | `false`                | Delete empty subfolders after moving (e.g. torrent folders). |
| `REVIEW_DIR`       | `/descargas/_revisar`  | Quarantine folder for doubtful matches. |
| `CACHE_FILE`       | `/config/cache.json`   | Series cache path. |
| `TZ`               | `Europe/Madrid`        | Container timezone (for Docker log timestamps). |

---

## Result examples

```
Peliculas/
├── El Padrino - Colección/
│   ├── El Padrino (1972)/El Padrino (1972).mkv
│   └── El Padrino: Parte II (1974)/El Padrino: Parte II (1974).mkv
└── Dune (2021)/Dune (2021).mkv

Series/
└── Severance (2022)/Season 01/Severance 1x01 ....mkv
```

---

## Synology (Container Manager)

You can either **build on the NAS** or **pull the prebuilt image** (faster, no
compilation). DSM 7.2+.

**Pull the image (recommended):**
1. Copy a folder with a `docker-compose.yml` based on `docker-compose.pull.yml`
   (set your `image:` and absolute `/volume1/...` paths) to e.g.
   `/volume1/docker/renombrador`.
2. Container Manager → **Project → Create** → point to that folder.

**Build on the NAS:**
1. Copy the whole repo (source + `Dockerfile` + `docker-compose.yml`) to the NAS.
2. Container Manager → **Project → Create** → it builds the image (first time
   takes a few minutes; needs internet and ~2 GB RAM, x86 NAS).

Typical Synology paths (`/volume1` may differ on your unit):
`/volume1/Descargas/Emule:/descargas`, `/volume1/Media/Peliculas:/peliculas`,
`/volume1/Media/Series:/series`, `/volume1/docker/renombrador/config:/config`.

---

## Publish your own image (GitHub Actions)

This repo includes `.github/workflows/docker-publish.yml`. On every push to
`main` it builds a multi-arch image (amd64/arm64) and publishes it:

- **GHCR** → `ghcr.io/<owner>/renombrador` (always, no setup).
- **Docker Hub** → `<user>/renombrador` — add two repo secrets first
  (*Settings → Secrets and variables → Actions*):
  - `DOCKERHUB_USERNAME` = your Docker Hub user.
  - `DOCKERHUB_TOKEN` = a Docker Hub access token (*Account → Security*).

---

## Operation

```bash
docker compose ps
docker compose logs -f
docker compose up -d --build      # rebuild after updating the code
docker compose down               # stop and remove
```

## Troubleshooting

| Symptom | Likely cause / fix |
|---|---|
| `[ERROR] missing TMDB_API_KEY` | Key not set in `.env`. |
| Nothing detected | Wrong `DOWNLOADS_DIR`? File below `MIN_FILE_MB`? Extension not `.mkv/.mp4/.avi`? |
| Slow to react | Normal: it waits `STABLE_SECS` + one `POLL_INTERVAL`. Lower them for more reactivity. |
| Everything goes to `_revisar` | Very messy names or wrong language. Lower `MATCH_THRESHOLD` or change `TMDB_LANGUAGE`; the log shows the score. |
| `TMDb no accesible … reintento N en ~Xs` | Network/TMDb outage (or invalid API key → `HTTP 401`). Files are left in place and retried automatically with backoff. |
| Wrong show matched | Edit/remove its entry in `/config/cache.json` and restart (stale ids that no longer exist in TMDb are dropped automatically). |
| `match ambiguo (score …) entre: …` | Several candidates are nearly tied (typical of franchises and remakes). The file goes to `_revisar` on purpose. Rename it adding the year — `Show (2022) 1x01.mkv` — and it resolves on the next pass. |
| `code: 101` when building | Toolchain too old; the `Dockerfile` uses `rust:1-slim-bookworm` (latest stable). |

---
---

# Español

[English](#renombrador-tmdb--docker-service) | **Español**

Versión **headless** (sin interfaz) de la app de escritorio. Corre como servicio
en Docker, **vigila tu carpeta de descargas**, y en cuanto detecta un vídeo nuevo
lo identifica en TMDb, lo **renombra** con el título oficial y lo **mueve** a tu
carpeta de películas o series, con la estructura que reconocen Plex/Jellyfin/Emby.

Reutiliza exactamente la misma lógica de parseo y búsqueda que la app de
escritorio (búsqueda progresiva multiidioma + puntuación de similitud), pero sin
intervención humana.

---

## Qué hace, paso a paso

1. Cada `POLL_INTERVAL` segundos recorre **recursivamente** la carpeta de
   descargas buscando archivos `.mkv`, `.mp4`, `.avi`.
2. Espera a que cada archivo esté **estable** (sin cambiar de tamaño durante
   `STABLE_SECS`) para no tocar descargas a medias.
3. Limpia el nombre de etiquetas de release (resolución, fuente, códec, grupo…)
   y extrae el título, el **año** de estreno y la temporada/episodio si los hay.
   También mira el texto que va *después* del código de episodio: en nombres
   como `Star Trek 1x01 Star Trek Strange New Worlds (2022)` el nombre real de
   la serie está ahí, y buscar solo `Star Trek` coincidía al 100 % con la serie
   de 1966. Un título de episodio normal se sigue ignorando.
4. Busca en TMDb el título oficial en tu idioma, pasando el año como filtro en
   vez de incrustarlo en la query. Si hay coincidencia fiable —score ≥
   `MATCH_THRESHOLD` **y** sin empate entre candidatos—:
   - **Película** → `Peliculas/Título (Año)/Título (Año).mkv`
     (y si pertenece a una saga, dentro de `Colección de TMDb/…`).
   - **Serie** → `Series/Serie (Año)/Season 01/Serie 1x01 Episodio.mkv`.
5. Si **no** está seguro, aplica la política de `ON_UNCERTAIN` (por defecto,
   mover a `descargas/_revisar/` para que lo mires a mano). Un empate cuenta
   como "no seguro" aunque el score sea 1.00: si varios candidatos quedan a
   menos de 0.02 entre sí, la decisión real la tomaría la popularidad, y eso no
   basta para renombrar sin supervisión. Solo se cachean los matches firmes.
6. Guarda en `/config/cache.json` el `id` de cada serie ya resuelta, para que los
   siguientes episodios se renombren al instante. Los nombres de episodio se
   piden **temporada a temporada** y se guardan en memoria: un lote con una
   temporada entera cuesta ~2 llamadas a TMDb en vez de 2 por capítulo.
7. Si TMDb **no responde** (red caída, timeout, error del servidor), los
   archivos se **dejan intactos** y se reintentan con espera creciente (2·, 4·,
   8·… el intervalo de sondeo, con techo de 15 min). Una caída de red nunca
   manda archivos a cuarentena.

> El movimiento es seguro: **nunca sobreescribe** un archivo distinto, y funciona
> aunque descargas y películas estén en **volúmenes diferentes** (copia a un
> archivo temporal `.part` y renombra al final, así un corte a media copia nunca
> deja un vídeo a medias en el destino).

> **Consejo — primer arranque:** pon `DRY_RUN=true` y mira el log. El servicio
> imprime el destino exacto que *usaría* para cada archivo sin mover nada.
> Cuando el resultado te convenza, vuelve a ponerlo en `false`.

---

## Dos formas de usarlo

- **A) Imagen ya compilada (recomendado):** descarga una imagen lista de Docker
  Hub / GHCR. No se compila nada en tu equipo/NAS.
- **B) Compilarla tú:** Docker compila el binario Rust dentro del contenedor.

### Conseguir la API key de TMDb (gratis)

1. Crea una cuenta en <https://www.themoviedb.org/signup>.
2. **Configuración → API** → *Crear* → *Desarrollador*.
3. Copia la **API Key (v3 auth)** en `TMDB_API_KEY`.

### A) Usar la imagen ya compilada

```bash
cp .env.example .env          # pon TMDB_API_KEY y las rutas del host
docker compose -f docker-compose.pull.yml up -d
docker compose -f docker-compose.pull.yml logs -f
```

(Edita la línea `image:` de `docker-compose.pull.yml` con tu imagen de Docker
Hub, p. ej. `tuusuario/renombrador:latest`, o usa `ghcr.io/letzzar/renombrador:latest`.)

### B) Compilarla tú

```bash
cp .env.example .env          # pon TMDB_API_KEY y las rutas del host
docker compose up -d --build
docker compose logs -f
```

Scripts de ayuda: `./scripts/start.sh`, `./scripts/logs.sh`, `./scripts/stop.sh`.

---

## Carpetas / montajes

| Dentro del contenedor | Variable del host  | Para qué sirve                          |
|-----------------------|--------------------|-----------------------------------------|
| `/descargas`          | `DOWNLOADS_DIR`    | Carpeta vigilada (entrada).             |
| `/peliculas`          | `MOVIES_HOST_DIR`  | Destino de películas.                   |
| `/series`             | `SERIES_HOST_DIR`  | Destino de series.                      |
| `/config`             | `CONFIG_DIR`       | Caché persistente (`cache.json`).       |

> Para que mover sea instantáneo, ten descargas/películas/series en el **mismo**
> sistema de archivos. Si están en discos distintos también funciona (copia y
> borra), solo que tarda más.

---

## Variables de entorno

| Variable           | Por defecto            | Descripción |
|--------------------|------------------------|-------------|
| `TMDB_API_KEY`     | — (**obligatoria**)    | Clave v3 de TMDb. |
| `TMDB_LANGUAGE`    | `es-ES`                | Idioma de títulos: `es-ES`, `es-MX`, `en-US`, `en-GB`, `fr-FR`, `de-DE`, `it-IT`, `pt-BR`. |
| `NESTED`           | `true`                 | `true` = estructura anidada Plex/Jellyfin; `false` = plana. |
| `YEAR_FORMAT`      | `parens`               | `parens` = `Título (2021)` (estándar Plex); `brackets` = `Título [2021]`. |
| `USE_COLLECTIONS`  | `true`                 | Agrupar sagas en una carpeta con el nombre de la colección de TMDb (solo anidado). |
| `EPISODE_FORMAT`   | `1x05`                 | `1x05` o `S01E05`. |
| `FORCE_MODE`       | `auto`                 | `auto` (detecta por nombre), `series` o `movies`. |
| `ON_UNCERTAIN`     | `revisar`              | Match dudoso: `revisar` (cuarentena), `dejar` (no tocar), `forzar` (usar el mejor). |
| `MATCH_THRESHOLD`  | `0.85`                 | Umbral de similitud [0..1] para renombrar sin dudar. |
| `DRY_RUN`          | `false`                | `true` = registrar lo que se *haría* (destinos calculados) sin mover nada. |
| `POLL_INTERVAL`    | `30`                   | Segundos entre cada revisión de la carpeta. |
| `STABLE_SECS`      | `60`                   | Segundos que un archivo debe estar quieto antes de procesarlo. |
| `MIN_FILE_MB`      | `50`                   | Tamaño mínimo (MB); descarta *samples*. |
| `CLEAN_EMPTY_DIRS` | `false`                | Borrar subcarpetas vacías tras mover (p. ej. carpetas de torrent). |
| `REVIEW_DIR`       | `/descargas/_revisar`  | Carpeta de cuarentena para matches dudosos. |
| `CACHE_FILE`       | `/config/cache.json`   | Ruta de la caché de series. |
| `TZ`               | `Europe/Madrid`        | Zona horaria (marcas de tiempo de los logs). |

---

## Ejemplos de resultado

```
Peliculas/
├── El Padrino - Colección/
│   ├── El Padrino (1972)/El Padrino (1972).mkv
│   └── El Padrino: Parte II (1974)/El Padrino: Parte II (1974).mkv
└── Dune (2021)/Dune (2021).mkv

Series/
└── Severance (2022)/Season 01/Severance 1x01 ....mkv
```

---

## Synology (Container Manager)

Puedes **compilar en el NAS** o **descargar la imagen ya compilada** (más rápido,
sin compilar). DSM 7.2+.

**Descargar la imagen (recomendado):**
1. Copia una carpeta con un `docker-compose.yml` basado en `docker-compose.pull.yml`
   (pon tu `image:` y las rutas absolutas `/volume1/...`) a, p. ej.,
   `/volume1/docker/renombrador`.
2. Container Manager → **Proyecto → Crear** → apunta a esa carpeta.

**Compilar en el NAS:**
1. Copia el repo entero (código + `Dockerfile` + `docker-compose.yml`) al NAS.
2. Container Manager → **Proyecto → Crear** → construye la imagen (la primera vez
   tarda unos minutos; necesita internet y ~2 GB de RAM, NAS x86).

Rutas típicas de Synology (`/volume1` puede variar en tu equipo):
`/volume1/Descargas/Emule:/descargas`, `/volume1/Media/Peliculas:/peliculas`,
`/volume1/Media/Series:/series`, `/volume1/docker/renombrador/config:/config`.

---

## Publicar tu propia imagen (GitHub Actions)

Este repo incluye `.github/workflows/docker-publish.yml`. En cada push a `main`
construye una imagen multiarch (amd64/arm64) y la publica:

- **GHCR** → `ghcr.io/<owner>/renombrador` (siempre, sin configurar nada).
- **Docker Hub** → `<usuario>/renombrador` — antes añade dos secrets al repo
  (*Settings → Secrets and variables → Actions*):
  - `DOCKERHUB_USERNAME` = tu usuario de Docker Hub.
  - `DOCKERHUB_TOKEN` = un Access Token de Docker Hub (*Account → Security*).

---

## Operación

```bash
docker compose ps
docker compose logs -f
docker compose up -d --build      # reconstruir tras actualizar el código
docker compose down               # parar y eliminar
```

## Resolución de problemas

| Síntoma | Causa probable / solución |
|---|---|
| `[ERROR] falta TMDB_API_KEY` | No definiste la clave en `.env`. |
| No detecta archivos | ¿`DOWNLOADS_DIR` correcta? ¿Supera `MIN_FILE_MB`? ¿Extensión `.mkv/.mp4/.avi`? |
| Tarda en procesar | Normal: espera `STABLE_SECS` + un ciclo de `POLL_INTERVAL`. Bájalos si quieres más reactividad. |
| Todo va a `_revisar` | Nombres muy sucios o idioma equivocado. Baja `MATCH_THRESHOLD` o cambia `TMDB_LANGUAGE`; el log muestra el score. |
| `TMDb no accesible … reintento N en ~Xs` | Caída de red/TMDb (o API key inválida → `HTTP 401`). Los archivos se dejan en su sitio y se reintentan solos con backoff. |
| Serie mal identificada | Borra/edita su entrada en `/config/cache.json` y reinicia (los ids obsoletos que ya no existen en TMDb se descartan solos). |
| `match ambiguo (score …) entre: …` | Varios candidatos casi empatados (típico de franquicias y remakes). El archivo va a `_revisar` a propósito. Renómbralo añadiendo el año — `Serie (2022) 1x01.mkv` — y se resuelve en la siguiente pasada. |
| `code: 101` al compilar | Toolchain demasiado antiguo; el `Dockerfile` usa `rust:1-slim-bookworm` (último estable). |

---

## Notas de arquitectura

- El crate produce dos binarios: `renombrador-daemon` (este servicio, lo que
  construye Docker) y `renombrador-gui` (la app de escritorio, feature `gui`).
- La lógica común (parseo, TMDb, movimiento, caché) vive en `src/lib.rs`.
- La imagen Docker **no** incluye dependencias gráficas: solo compila el daemon,
  y usa TLS **rustls** (sin OpenSSL del sistema).
