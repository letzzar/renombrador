//! Operaciones de sistema de archivos seguras.

use std::fs;
use std::path::Path;

/// Ajuste de permisos del resultado final. Solo tiene efecto en Unix.
///
/// `fs::rename` conserva el modo del archivo de origen y `fs::copy` lo copia
/// también, así que sin esto un archivo que llega a descargas en `600` (o peor,
/// `000`) se deposita igual en la carpeta de series o películas. Da igual
/// mientras quien reproduce es el propietario, pero rompe cualquier consumidor
/// que llegue por NFS o desde un contenedor con otro UID, que solo puede
/// apoyarse en los bits de "otros".
#[cfg(unix)]
mod permisos {
    use std::fs;
    use std::path::Path;
    use std::sync::OnceLock;

    const MODO_ARCHIVO_POR_DEFECTO: u32 = 0o644;
    const MODO_DIR_POR_DEFECTO: u32 = 0o755;

    /// Lee un modo en octal de una variable de entorno (`644`, `0644` y `0o644`
    /// son equivalentes). Si falta o no es válida se usa `def`.
    fn modo_env(clave: &str, def: u32) -> u32 {
        std::env::var(clave)
            .ok()
            .and_then(|v| {
                let v = v.trim();
                let v = v.strip_prefix("0o").or_else(|| v.strip_prefix("0O")).unwrap_or(v);
                u32::from_str_radix(v, 8).ok()
            })
            .filter(|m| *m <= 0o7777)
            .unwrap_or(def)
    }

    pub fn modo_archivo() -> u32 {
        static M: OnceLock<u32> = OnceLock::new();
        *M.get_or_init(|| modo_env("FILE_MODE", MODO_ARCHIVO_POR_DEFECTO))
    }

    pub fn modo_dir() -> u32 {
        static M: OnceLock<u32> = OnceLock::new();
        *M.get_or_init(|| modo_env("DIR_MODE", MODO_DIR_POR_DEFECTO))
    }

    /// Aplica `modo` a `ruta`. Los fallos no son fatales: el archivo ya está en
    /// su sitio y en un montaje que no soporte `chmod` (o sin ser propietario)
    /// no hay nada que podamos hacer.
    pub fn aplicar(ruta: &Path, modo: u32) {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(ruta, fs::Permissions::from_mode(modo));
    }
}

/// Modos (archivo, directorio) que se aplicarán al resultado final, para
/// poder mostrarlos en el arranque. `None` en plataformas sin permisos Unix.
#[cfg(unix)]
pub fn modos_configurados() -> Option<(u32, u32)> {
    Some((permisos::modo_archivo(), permisos::modo_dir()))
}

/// Windows no tiene modos POSIX: no hay nada que normalizar ni que anunciar.
#[cfg(not(unix))]
pub fn modos_configurados() -> Option<(u32, u32)> {
    None
}

/// Deja el archivo con el modo configurado (`FILE_MODE`, por defecto 644).
#[cfg(unix)]
fn normalizar_archivo(ruta: &Path) {
    permisos::aplicar(ruta, permisos::modo_archivo());
}

#[cfg(not(unix))]
fn normalizar_archivo(_ruta: &Path) {}

/// Deja el directorio con el modo configurado (`DIR_MODE`, por defecto 755).
#[cfg(unix)]
fn normalizar_dir(ruta: &Path) {
    permisos::aplicar(ruta, permisos::modo_dir());
}

#[cfg(not(unix))]
fn normalizar_dir(_ruta: &Path) {}

/// Crea `dir` y sus padres dejando con el modo configurado (`DIR_MODE`, por
/// defecto 755) **solo los niveles que no existían**. Los directorios ya
/// creados no se tocan, para no alterar permisos que el usuario haya fijado.
pub fn crear_dirs(dir: &Path) -> std::io::Result<()> {
    // Anotamos qué niveles faltan antes de crearlos: `create_dir_all` no dice
    // cuáles ha creado y aplica `0o777 & !umask`, que según la umask heredada
    // puede dejarlos sin `o+rx`.
    let mut nuevos = Vec::new();
    let mut actual = Some(dir);
    while let Some(d) = actual {
        if d.as_os_str().is_empty() || d.exists() {
            break;
        }
        nuevos.push(d);
        actual = d.parent();
    }

    fs::create_dir_all(dir)?;

    for d in nuevos.iter().rev() {
        normalizar_dir(d);
    }

    Ok(())
}

/// Renombra `origen` a `destino` dentro del mismo directorio sin sobreescribir
/// un archivo distinto preexistente. (Usado por la GUI.)
pub fn renombrar_si_seguro(origen: &Path, destino: &Path) -> Result<(), String> {
    if destino.exists() {
        let mismo_archivo = fs::canonicalize(origen)
            .ok()
            .zip(fs::canonicalize(destino).ok())
            .map(|(o, d)| o == d)
            .unwrap_or(false);
        if !mismo_archivo {
            let nombre_dest = destino
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            return Err(format!(
                "el destino '{}' ya existe; se omite para no sobreescribirlo",
                nombre_dest
            ));
        }
    }
    fs::rename(origen, destino).map_err(|e| e.to_string())
}

/// Mueve un archivo a `destino`, creando los directorios necesarios y
/// funcionando también **entre sistemas de archivos distintos** (típico en
/// Docker: la carpeta de descargas y la de películas pueden ser bind-mounts a
/// volúmenes diferentes, donde `rename(2)` falla con `EXDEV`).
///
/// No sobreescribe un archivo distinto que ya exista en el destino.
pub fn mover_seguro(origen: &Path, destino: &Path) -> Result<(), String> {
    if let Some(padre) = destino.parent() {
        crear_dirs(padre)
            .map_err(|e| format!("no se pudo crear '{}': {}", padre.display(), e))?;
    }

    if destino.exists() {
        let mismo_archivo = fs::canonicalize(origen)
            .ok()
            .zip(fs::canonicalize(destino).ok())
            .map(|(o, d)| o == d)
            .unwrap_or(false);
        if mismo_archivo {
            return Ok(()); // ya está en su sitio
        }
        return Err(format!(
            "el destino '{}' ya existe; se omite para no sobreescribirlo",
            destino.display()
        ));
    }

    // Primero intentamos un rename (rápido, atómico). Si falla —probablemente
    // por estar en otro sistema de archivos— copiamos y borramos el origen.
    if fs::rename(origen, destino).is_ok() {
        normalizar_archivo(destino);
        return Ok(());
    }

    // Copiamos a un temporal `.part` junto al destino y renombramos al final:
    // si el proceso muere a mitad de copia nunca queda un destino parcial que
    // parezca un vídeo válido (y que bloquearía reintentos por "ya existe").
    let mut nombre_tmp = destino
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    nombre_tmp.push(".part");
    let tmp = destino.with_file_name(nombre_tmp);

    fs::copy(origen, &tmp).map_err(|e| {
        // Si la copia falló a medias, intentamos no dejar basura.
        let _ = fs::remove_file(&tmp);
        format!("error al copiar a '{}': {}", tmp.display(), e)
    })?;
    // Ajustamos los permisos sobre el `.part`, antes del rename final: así el
    // archivo nunca llega a existir con su nombre definitivo y permisos malos,
    // y un escaneo de la biblioteca no puede pillarlo en ese estado.
    normalizar_archivo(&tmp);
    fs::rename(&tmp, destino).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("error al renombrar '{}' a su nombre final: {}", tmp.display(), e)
    })?;
    fs::remove_file(origen)
        .map_err(|e| format!("copiado correctamente pero no se pudo borrar el origen: {}", e))?;
    Ok(())
}

/// Borra `dir` si está vacío y es un subdirectorio de `raiz` (nunca borra la
/// raíz). Útil para limpiar las carpetas que dejan los clientes torrent.
pub fn borrar_dir_vacio_bajo(dir: &Path, raiz: &Path) {
    if dir == raiz {
        return;
    }
    if !dir.starts_with(raiz) {
        return;
    }
    if let Ok(mut it) = fs::read_dir(dir) {
        if it.next().is_none() {
            let _ = fs::remove_dir(dir);
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn modo(p: &Path) -> u32 {
        fs::metadata(p).unwrap().permissions().mode() & 0o7777
    }

    fn caja(nombre: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "renombrador-test-{}-{}",
            nombre,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// El bug original: un archivo que llega a descargas sin permisos se
    /// depositaba tal cual en el destino, ilegible para Jellyfin vía NFS.
    #[test]
    fn mover_normaliza_los_permisos_del_archivo() {
        let raiz = caja("mover");
        let origen = raiz.join("origen.mkv");
        fs::write(&origen, b"video").unwrap();
        fs::set_permissions(&origen, fs::Permissions::from_mode(0o000)).unwrap();

        let destino = raiz.join("Serie (2025)/Season 01/Serie 1x01.mkv");
        mover_seguro(&origen, &destino).unwrap();

        assert_eq!(modo(&destino), 0o644);
        let _ = fs::remove_dir_all(&raiz);
    }

    /// El mismo caso cruzando de sistema de archivos (bind-mounts distintos en
    /// Docker), que va por copia a `.part` en vez de por `rename`. Usa `600` en
    /// el origen porque el test no corre como root y tiene que poder leerlo.
    #[test]
    fn mover_entre_sistemas_de_archivos_normaliza_los_permisos() {
        // /dev/shm es tmpfs; el directorio temporal, normalmente disco.
        let otro_fs = Path::new("/dev/shm");
        if !otro_fs.is_dir() {
            return;
        }
        let origen_dir = otro_fs.join(format!("renombrador-test-exdev-{}", std::process::id()));
        let _ = fs::remove_dir_all(&origen_dir);
        if fs::create_dir_all(&origen_dir).is_err() {
            return;
        }
        let raiz = caja("exdev");

        let origen = origen_dir.join("origen.mkv");
        fs::write(&origen, b"video").unwrap();
        fs::set_permissions(&origen, fs::Permissions::from_mode(0o600)).unwrap();

        // Comprobamos con un archivo aparte que las dos rutas están de verdad
        // en sistemas de archivos distintos; si no, este test no ejercitaría el
        // camino de copia y no afirmamos nada.
        let sonda = origen_dir.join("sonda");
        fs::write(&sonda, b"x").unwrap();
        let cruza_fs = fs::rename(&sonda, raiz.join("sonda")).is_err();
        let _ = fs::remove_file(&sonda);
        let _ = fs::remove_file(raiz.join("sonda"));
        if !cruza_fs {
            let _ = fs::remove_dir_all(&origen_dir);
            let _ = fs::remove_dir_all(&raiz);
            return;
        }

        let destino = raiz.join("Serie (2025)/Season 01/Serie 1x01.mkv");
        mover_seguro(&origen, &destino).unwrap();

        assert_eq!(modo(&destino), 0o644);
        assert!(!origen.exists(), "el origen se borra tras copiar");
        assert!(
            !destino.with_file_name("Serie 1x01.mkv.part").exists(),
            "no debe quedar el temporal"
        );
        let _ = fs::remove_dir_all(&origen_dir);
        let _ = fs::remove_dir_all(&raiz);
    }

    /// Los niveles que creamos nosotros salen legibles y atravesables; los que
    /// ya existían conservan sus permisos.
    #[test]
    fn crear_dirs_solo_toca_los_niveles_nuevos() {
        let raiz = caja("dirs");
        fs::set_permissions(&raiz, fs::Permissions::from_mode(0o700)).unwrap();

        let hoja = raiz.join("Serie (2025)/Season 01");
        crear_dirs(&hoja).unwrap();

        assert_eq!(modo(&raiz), 0o700, "un directorio preexistente no se toca");
        assert_eq!(modo(&raiz.join("Serie (2025)")), 0o755);
        assert_eq!(modo(&hoja), 0o755);
        let _ = fs::remove_dir_all(&raiz);
    }
}
