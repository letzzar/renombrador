//! Operaciones de sistema de archivos seguras.

use std::fs;
use std::path::Path;

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
        fs::create_dir_all(padre)
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
        return Ok(());
    }

    fs::copy(origen, destino).map_err(|e| {
        // Si la copia falló a medias, intentamos no dejar basura.
        let _ = fs::remove_file(destino);
        format!("error al copiar a '{}': {}", destino.display(), e)
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
