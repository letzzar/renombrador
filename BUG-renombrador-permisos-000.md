# Bug: `renombrador-tmdb` deja los ficheros finales con permisos `000`

## Resumen

Tras procesar y renombrar contenido, los ficheros de vídeo resultantes quedan en
`----------` (modo `000`) en el volumen de destino. Ningún proceso puede leerlos,
lo que rompe la reproducción en Jellyfin con `Permission denied` al abrir el fichero.

**Los directorios creados sí quedan correctos (`drwxrwxrwx`). Solo fallan los ficheros.**

---

## Evidencia recogida

### Estado observado en destino

```
$ ls -l "/media/Series/Pluribus (2025)/Season 01/"
---------- 1 nobody nogroup 2583074139 Aug 31 17:40 'Pluribus 1x01 Nosotros es nosotros.mkv'
---------- 1 nobody nogroup 2594574547 Aug 31 17:40 'Pluribus 1x02 La pirata.mkv'
---------- 1 nobody nogroup 1609634208 Aug 31 17:40 'Pluribus 1x03 La granada.mkv'
[... resto igual ...]

$ ls -ld "/media/Series/Pluribus (2025)" "/media/Series/Pluribus (2025)/Season 01"
drwxrwxrwx 1 nobody nogroup  18 Aug 31 17:29 '/media/Series/Pluribus (2025)'
drwxrwxrwx 1 nobody nogroup 558 Aug 31 17:48 '/media/Series/Pluribus (2025)/Season 01'
```

> `nobody nogroup` es normal: el destino se ve a través de NFS desde un LXC
> unprivileged. No es parte del bug.

### Alcance

```
$ find /media -type f ! -perm -o+r | wc -l
350
$ find /media -type f -perm -o+r | wc -l
45903
```

Solo están afectados los ficheros procesados recientemente por el renombrador.
Los ~45.900 anteriores se leen sin problema, lo que descarta un fallo del montaje NFS
o del contenedor de Jellyfin.

### Error resultante en el consumidor (Jellyfin)

```
[in#0 @ 0x...] Error opening input: Permission denied
Error opening input file file:/media/Series/Pluribus (2025)/Season 01/Pluribus 1x06 HDP.mkv.
Error opening input files: Permission denied
```

El `MediaSourceInfo` llega con `"MediaStreams":[]` — ffprobe tampoco pudo abrir el fichero.

---

## Deducción clave: NO es la `umask`

Este es el punto más importante para orientar la búsqueda en el código.

- Si la `umask` del proceso fuese `0777`, **los directorios también saldrían en `000`**.
- Los directorios salen en `777`, es decir, la `umask` efectiva es `0` (totalmente permisiva).
- Por tanto el `000` de los ficheros **no viene de la umask heredada**, sino de algo que
  actúa específicamente sobre la ruta del fichero.

Esto reduce mucho el espacio de búsqueda.

---

## Hipótesis a revisar, por probabilidad

### 1. Preservación de permisos del origen (la más probable)

`shutil.move()`, `shutil.copy2()` y `shutil.copystat()` **preservan el modo del fichero
origen**. Si los ficheros que llegan a `WATCH_DIR` (`/descargas`) ya vienen con permisos
restrictivos desde el cliente de descarga, el renombrador los estaría propagando intactos
al destino sin ser el causante original.

**Comprobación (ejecutar en el NAS):**

```bash
ls -l /volume1/<ruta_real_de_descargas>/ | head -20
stat -c '%a %n' /volume1/<ruta_real_de_descargas>/* | head -20
```

Si ahí ya aparecen en `000`, el renombrador solo hereda el problema — pero la corrección
sigue teniendo que estar en él (ver Fix 1), porque es quien deposita el fichero final.

### 2. `os.chmod()` explícito con valor erróneo

Buscar en el código cualquier llamada a `chmod`:

```bash
grep -rn "chmod\|copystat\|copymode\|copy2\|shutil.move\|os.rename" .
```

Errores típicos:
- `os.chmod(dst, 0)` o `os.chmod(dst, 0o000)`
- Pasar el modo en decimal en vez de octal (`os.chmod(dst, 644)` → `644` decimal = `0o1204`,
  resultado inesperado)
- Construir el modo con una máscara mal calculada (`mode & ~0o777`)

### 3. `os.open()` / `os.mkdir()` con `mode` explícito a 0

Si el fichero se escribe con `os.open(path, flags, mode)` y `mode=0`, el fichero nace sin
permisos independientemente de la umask.

### 4. `os.umask()` llamado dentro del proceso

Aunque la deducción de arriba lo hace improbable, conviene descartar que haya una llamada
`os.umask(0o777)` **acotada** a la sección que escribe ficheros (y no a la que crea
directorios), lo que explicaría la asimetría:

```bash
grep -rn "umask" .
```

### 5. Escritura vía fichero temporal + rename

Si el script escribe a un `.tmp` / `.part` con `tempfile.mkstemp()` y luego hace `os.rename()`:
`mkstemp()` crea el fichero con modo `0o600` por diseño, y si además hay un `chmod`
posterior mal calculado puede acabar en `000`. El `os.rename` **no** ajusta permisos.

---

## Correcciones propuestas

### Fix 1 — Normalizar permisos explícitamente tras cada operación (recomendado)

Es la corrección robusta: no depende de la umask, ni del origen, ni del método de copia.
Aplicar inmediatamente después de mover/renombrar cada fichero y de crear cada directorio.

```python
import os

FILE_MODE = int(os.environ.get("FILE_MODE", "644"), 8)   # 0o644
DIR_MODE  = int(os.environ.get("DIR_MODE",  "755"), 8)   # 0o755

def normalizar_permisos(ruta: str) -> None:
    """Deja el fichero o directorio legible por todos. Idempotente."""
    try:
        if os.path.isdir(ruta):
            os.chmod(ruta, DIR_MODE)
        else:
            os.chmod(ruta, FILE_MODE)
    except OSError as e:
        log.warning("No se pudo ajustar permisos de %s: %s", ruta, e)
```

Llamarla en el punto donde el fichero queda en su ubicación definitiva:

```python
shutil.move(origen, destino)
normalizar_permisos(destino)
normalizar_permisos(os.path.dirname(destino))
```

Si se crean directorios intermedios, normalizar también cada nivel creado
(`os.makedirs` aplica la umask solo al último nivel de forma fiable).

### Fix 2 — Fijar la umask al arranque

Complementario, no sustituto del Fix 1:

```python
import os
os.umask(0o022)   # ficheros 644, directorios 755
```

Colocarlo lo antes posible en el `main()`, antes de cualquier escritura.

### Fix 3 — Usar `shutil.move` sin arrastrar metadatos

Si se está usando `copy2` o `copystat`, cambiar a `shutil.copy` (que no copia el modo)
o mantener `copy2` pero forzando el `chmod` posterior del Fix 1.

### Fix 4 — Soporte de `UMASK` / `PUID` / `PGID` como variables de entorno

Para alinearse con la convención habitual en imágenes de medios y poder ajustarlo sin
recompilar. Variables actuales del contenedor (para referencia):

```
NESTED=true          MIN_FILE_MB=50       USE_COLLECTIONS=true
CLEAN_EMPTY_DIRS=true POLL_INTERVAL=30    TMDB_LANGUAGE=es-ES
YEAR_FORMAT=parens   TZ=Europe/Madrid     FORCE_MODE=auto
STABLE_SECS=60       DRY_RUN=false        ON_UNCERTAIN=revisar
MATCH_THRESHOLD=0.85 EPISODE_FORMAT=1x05
WATCH_DIR=/descargas MOVIES_DIR=/peliculas SERIES_DIR=/series
CACHE_FILE=/config/cache.json
```

No hay `UMASK`, `PUID` ni `PGID`, así que el contenedor corre como root con la umask
por defecto de la imagen base.

### Fix 5 — Parche a nivel Docker (si no se toca el código ahora)

Envolver el arranque forzando la umask en el `ENTRYPOINT` / `CMD`:

```dockerfile
ENTRYPOINT ["/bin/sh", "-c", "umask 022 && exec python /app/renombrador.py"]
```

O en `docker-compose.yml`:

```yaml
entrypoint: ["/bin/sh", "-c", "umask 022 && exec python /app/renombrador.py"]
```

> Ojo: esto solo funciona si el `000` proviene de la umask, cosa que la deducción de arriba
> hace improbable. Si el origen es un `chmod` explícito o la herencia desde `/descargas`,
> este parche **no** resolverá nada. Priorizar el Fix 1.

---

## Mitigación temporal (no sustituye al arreglo)

Corrección manual de lo ya afectado, ejecutar **por SSH en el NAS**, no desde el LXC
(el montaje NFS está en solo lectura):

```bash
sudo find /volume1/Media -type f ! -perm -o+r -exec chmod a+r {} \;
sudo find /volume1/Media -type d ! -perm -o+rx -exec chmod a+rx {} \;
```

Opcionalmente, como red de seguridad hasta que el bug esté cerrado, programar eso mismo
en DSM (Panel de control → Programador de tareas → Script definido por el usuario, como root).
El filtro `! -perm` hace que solo toque lo que está mal, así que el coste es despreciable.

---

## Criterio de aceptación

Tras el arreglo, procesar contenido nuevo y verificar:

```bash
# en el LXC de Jellyfin
find /media -type f ! -perm -o+r | wc -l     # debe devolver 0
```

Y que los ficheros recién creados aparezcan como `-rw-r--r--` (o al menos con `r` en "otros"),
con los directorios en `drwxr-xr-x` o superior.

---

## Contexto de la infraestructura (para entender el flujo completo)

```
[cliente de descarga] → /descargas (WATCH_DIR)
        ↓
[renombrador-tmdb, Docker en Synology DS1621+, corre como root]
        ↓
/peliculas y /series  →  /volume1/Media/... en el NAS
        ↓  (export NFS v3, solo lectura, squash a admin)
[host Proxmox: /mnt/nas-media]
        ↓  (bind mount mp0, ro=1)
[LXC unprivileged 105: /media]
        ↓
[Jellyfin nativo, usuario jellyfin, lee los ficheros]
```

El acceso desde el LXC depende de los bits de "otros" (`o+r` en ficheros, `o+rx` en
directorios), porque el mapeo de UIDs del contenedor unprivileged hace que el propietario
real no coincida con ningún usuario local. Por eso un fichero en `000` es directamente
ilegible, aunque toda la cadena de montaje esté correcta.
