//! Operaciones de sistema de archivos seguras.

use std::fs;
use std::path::Path;

/// Ajuste de permisos y propietario del resultado final. Solo tiene efecto en
/// Unix.
///
/// `fs::rename` conserva el modo y el dueño del archivo de origen, y `fs::copy`
/// se trae el modo, así que sin esto un archivo que llega a descargas en `600`
/// (o peor, `000`) se deposita igual en la carpeta de series o películas. Da
/// igual mientras quien reproduce es el propietario, pero rompe cualquier
/// consumidor que llegue por NFS o desde un contenedor con otro UID, que solo
/// puede apoyarse en los bits de "otros".
///
/// **No hay modo por defecto**: lo que se deposita se pone al nivel de la
/// biblioteca a la que llega. La referencia es el directorio de destino —el
/// primer nivel que ya existe bajo `/series` o `/peliculas`—, del que se copian
/// modo y `uid:gid`. Un archivo sale con los mismos permisos que sus vecinos y
/// con el dueño de la carpeta que lo acoge, así que el resultado es
/// indistinguible del contenido que ya se reproduce bien. Deliberadamente NO se
/// hereda del origen: el archivo de descargas es justo el que llega en `000`.
/// `FILE_MODE`/`DIR_MODE`/`PUID`/`PGID` sobreescriben lo que haga falta.
#[cfg(unix)]
mod permisos {
    use std::fs;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;
    use std::sync::OnceLock;

    /// Lee un modo en octal de una variable de entorno (`664`, `0664` y `0o664`
    /// son equivalentes). `None` si falta o no es válida: entonces manda lo que
    /// diga el directorio de destino.
    fn modo_env(clave: &str) -> Option<u32> {
        std::env::var(clave)
            .ok()
            .and_then(|v| {
                let v = v.trim();
                let v = v.strip_prefix("0o").or_else(|| v.strip_prefix("0O")).unwrap_or(v);
                u32::from_str_radix(v, 8).ok()
            })
            .filter(|m| *m <= 0o7777)
    }

    /// Modos forzados por entorno (archivo, directorio).
    pub fn modos_forzados() -> (Option<u32>, Option<u32>) {
        static M: OnceLock<(Option<u32>, Option<u32>)> = OnceLock::new();
        *M.get_or_init(|| (modo_env("FILE_MODE"), modo_env("DIR_MODE")))
    }

    /// Lee un identificador numérico de usuario o de grupo (`PUID`/`PGID`).
    fn id_env(clave: &str) -> Option<u32> {
        std::env::var(clave)
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
    }

    /// UID/GID forzados por entorno. Las dos mitades son independientes: se
    /// puede fijar solo el grupo y dejar que el usuario siga heredándose.
    pub fn ids_forzados() -> (Option<u32>, Option<u32>) {
        static F: OnceLock<(Option<u32>, Option<u32>)> = OnceLock::new();
        *F.get_or_init(|| (id_env("PUID"), id_env("PGID")))
    }

    /// Modo y propietario de `ruta`, si se puede leer su metadata.
    pub fn estado_de(ruta: &Path) -> Option<(u32, u32, u32)> {
        fs::metadata(ruta)
            .ok()
            .map(|m| (m.mode() & 0o7777, m.uid(), m.gid()))
    }

    /// Aplica propietario y modo a `ruta`. Cada mitad es opcional: sin dato que
    /// aplicar no se toca nada, que es preferible a inventarse un valor.
    ///
    /// Los fallos no son fatales: el archivo ya está en su sitio, y ni el
    /// `chown` (que exige ser root o tener CAP_CHOWN) ni el `chmod` pueden
    /// hacer nada en un montaje que no los soporte.
    pub fn aplicar(ruta: &Path, modo: Option<u32>, uid: Option<u32>, gid: Option<u32>) {
        use std::os::unix::fs::PermissionsExt;
        // El chown va antes que el chmod: cambiar de propietario limpia los
        // bits setuid/setgid, así que al revés desharía un modo tipo 2775.
        if uid.is_some() || gid.is_some() {
            let _ = std::os::unix::fs::chown(ruta, uid, gid);
        }
        if let Some(modo) = modo {
            let _ = fs::set_permissions(ruta, fs::Permissions::from_mode(modo));
        }
    }
}

/// Permisos y propietario que hereda lo que depositamos, leídos del directorio
/// de destino.
///
/// Se resuelve **antes** de crear nada: en cuanto `create_dir_all` añade
/// niveles nuevos, el "primer nivel que ya existe" pasa a ser uno recién
/// creado y la referencia se perdería. En Windows no hay permisos POSIX, así
/// que la estructura queda vacía y todo su uso se compila a nada.
#[derive(Clone, Copy, Default)]
pub struct Herencia {
    #[cfg(unix)]
    modo_archivo: Option<u32>,
    #[cfg(unix)]
    modo_dir: Option<u32>,
    #[cfg(unix)]
    uid: Option<u32>,
    #[cfg(unix)]
    gid: Option<u32>,
}

impl Herencia {
    /// Toma como referencia el primer ancestro de `destino` que ya existe
    /// (`destino` mismo si existe): la carpeta de la biblioteca en la que
    /// aterriza el archivo.
    ///
    /// - Los directorios nuevos copian su modo tal cual, bits setgid incluidos
    ///   (un `2775` en la raíz de la biblioteca debe seguir propagándose).
    /// - Los archivos copian solo los bits de lectura y escritura (`& 0o666`):
    ///   un `.mkv` no es ejecutable, y setuid/setgid en un archivo significan
    ///   algo muy distinto que en una carpeta.
    /// - `uid`/`gid` salen de esa misma carpeta, para que el archivo quede del
    ///   mismo dueño y grupo que el resto de la biblioteca.
    #[cfg(unix)]
    pub fn del_destino(destino: &Path) -> Self {
        let (modo_archivo_env, modo_dir_env) = permisos::modos_forzados();
        let (uid_env, gid_env) = permisos::ids_forzados();

        let mut referencia = Some(destino);
        let estado = loop {
            let Some(r) = referencia else { break None };
            if r.as_os_str().is_empty() {
                break None;
            }
            if let Some(estado) = permisos::estado_de(r) {
                break Some(estado);
            }
            referencia = r.parent();
        };

        let (modo, uid, gid) = match estado {
            Some((m, u, g)) => (Some(m), Some(u), Some(g)),
            None => (None, None, None),
        };

        Self {
            modo_archivo: modo_archivo_env.or(modo.map(|m| m & 0o666)),
            modo_dir: modo_dir_env.or(modo),
            uid: uid_env.or(uid),
            gid: gid_env.or(gid),
        }
    }

    #[cfg(not(unix))]
    pub fn del_destino(_destino: &Path) -> Self {
        Self::default()
    }
}

/// Resumen de la política de permisos, para anunciarla en el arranque. `None`
/// en plataformas sin permisos Unix.
#[cfg(unix)]
pub fn descripcion_permisos() -> Option<String> {
    let (modo_archivo, modo_dir) = permisos::modos_forzados();
    let (uid, gid) = permisos::ids_forzados();

    let mut forzado = Vec::new();
    if let Some(m) = modo_archivo {
        forzado.push(format!("archivos {:o}", m));
    }
    if let Some(m) = modo_dir {
        forzado.push(format!("directorios {:o}", m));
    }
    if let Some(u) = uid {
        forzado.push(format!("uid {}", u));
    }
    if let Some(g) = gid {
        forzado.push(format!("gid {}", g));
    }

    Some(if forzado.is_empty() {
        "heredados del directorio de destino".to_string()
    } else {
        format!("{} (el resto, del destino)", forzado.join(" · "))
    })
}

/// Windows no tiene modos POSIX: no hay nada que normalizar ni que anunciar.
#[cfg(not(unix))]
pub fn descripcion_permisos() -> Option<String> {
    None
}

/// Deja el archivo con los permisos y el dueño del directorio de destino.
#[cfg(unix)]
fn normalizar_archivo(ruta: &Path, h: Herencia) {
    permisos::aplicar(ruta, h.modo_archivo, h.uid, h.gid);
}

#[cfg(not(unix))]
fn normalizar_archivo(_ruta: &Path, _h: Herencia) {}

/// Deja el directorio con los permisos y el dueño del nivel del que cuelga.
#[cfg(unix)]
fn normalizar_dir(ruta: &Path, h: Herencia) {
    permisos::aplicar(ruta, h.modo_dir, h.uid, h.gid);
}

#[cfg(not(unix))]
fn normalizar_dir(_ruta: &Path, _h: Herencia) {}

/// Crea `dir` y sus padres dejando **solo los niveles que no existían** con el
/// modo y el dueño del primer nivel que sí existía. Los directorios ya creados
/// no se tocan, para no alterar permisos que el usuario haya fijado.
pub fn crear_dirs(dir: &Path) -> std::io::Result<()> {
    crear_dirs_con(dir, Herencia::del_destino(dir))
}

fn crear_dirs_con(dir: &Path, herencia: Herencia) -> std::io::Result<()> {
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
        normalizar_dir(d, herencia);
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
    // La referencia se lee ANTES de crear nada: si primero creásemos
    // `Serie (2025)/Season 01`, el "primer nivel que ya existe" sería una
    // carpeta recién hecha (con la umask del proceso) en vez de la raíz de la
    // biblioteca, y estaríamos heredando de nosotros mismos.
    let herencia = Herencia::del_destino(destino);

    if let Some(padre) = destino.parent() {
        crear_dirs_con(padre, herencia)
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
        normalizar_archivo(destino, herencia);
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
    // Ajustamos permisos y propietario sobre el `.part`, antes del rename
    // final: así el archivo nunca llega a existir con su nombre definitivo y
    // permisos malos, y un escaneo de la biblioteca no puede pillarlo así.
    normalizar_archivo(&tmp, herencia);
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
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn modo(p: &Path) -> u32 {
        fs::metadata(p).unwrap().permissions().mode() & 0o7777
    }

    fn duenio(p: &Path) -> (u32, u32) {
        let m = fs::metadata(p).unwrap();
        (m.uid(), m.gid())
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
    /// Ahora sale al nivel de la carpeta que lo acoge: `775` de la biblioteca
    /// menos los bits de ejecución, que en un `.mkv` no pintan nada.
    #[test]
    fn mover_pone_el_archivo_al_nivel_de_la_biblioteca() {
        let raiz = caja("mover");
        let origen = raiz.join("origen.mkv");
        fs::write(&origen, b"video").unwrap();
        fs::set_permissions(&origen, fs::Permissions::from_mode(0o000)).unwrap();

        let biblioteca = raiz.join("series");
        fs::create_dir_all(&biblioteca).unwrap();
        fs::set_permissions(&biblioteca, fs::Permissions::from_mode(0o775)).unwrap();

        let destino = biblioteca.join("Serie (2025)/Season 01/Serie 1x01.mkv");
        mover_seguro(&origen, &destino).unwrap();

        assert_eq!(modo(&destino), 0o664, "0o775 & 0o666");
        assert_eq!(
            modo(destino.parent().unwrap()),
            0o775,
            "las carpetas sí conservan los bits de ejecución"
        );
        let _ = fs::remove_dir_all(&raiz);
    }

    /// El dueño y el grupo salen de la carpeta de destino, no del proceso ni
    /// del origen. El test no corre como root, así que no puede fabricar una
    /// biblioteca de otro dueño: lo que fija es que se lee el `uid:gid` del
    /// destino y se aplica al resultado (con root y una biblioteca ajena, ese
    /// mismo camino es el que ejecuta el `chown`).
    #[test]
    fn mover_hereda_el_duenio_del_destino() {
        let raiz = caja("duenio");
        let origen = raiz.join("origen.mkv");
        fs::write(&origen, b"video").unwrap();

        let biblioteca = raiz.join("series");
        fs::create_dir_all(&biblioteca).unwrap();
        let esperado = duenio(&biblioteca);

        let destino = biblioteca.join("Serie (2025)/Season 01/Serie 1x01.mkv");
        mover_seguro(&origen, &destino).unwrap();

        assert_eq!(duenio(&destino), esperado, "el archivo hereda el dueño");
        assert_eq!(
            duenio(destino.parent().unwrap()),
            esperado,
            "y también las carpetas que creamos para él"
        );
        let _ = fs::remove_dir_all(&raiz);
    }

    /// El setgid de la raíz de una biblioteca (`2775`, el truco para que todo
    /// lo nuevo caiga en el mismo grupo) debe seguir propagándose a las
    /// carpetas, pero NO colarse en un archivo, donde significa otra cosa.
    #[test]
    fn el_setgid_se_propaga_a_carpetas_pero_no_a_archivos() {
        let raiz = caja("setgid");
        let origen = raiz.join("origen.mkv");
        fs::write(&origen, b"video").unwrap();

        let biblioteca = raiz.join("series");
        fs::create_dir_all(&biblioteca).unwrap();
        fs::set_permissions(&biblioteca, fs::Permissions::from_mode(0o2775)).unwrap();
        if modo(&biblioteca) != 0o2775 {
            // Hay sistemas de archivos que no dejan fijar el setgid; ahí este
            // test no afirmaría nada.
            let _ = fs::remove_dir_all(&raiz);
            return;
        }

        let destino = biblioteca.join("Serie (2025)/Season 01/Serie 1x01.mkv");
        mover_seguro(&origen, &destino).unwrap();

        assert_eq!(modo(destino.parent().unwrap()), 0o2775);
        assert_eq!(modo(&destino), 0o664, "sin setgid: 0o2775 & 0o666");
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

        let biblioteca = raiz.join("series");
        fs::create_dir_all(&biblioteca).unwrap();
        fs::set_permissions(&biblioteca, fs::Permissions::from_mode(0o750)).unwrap();

        let destino = biblioteca.join("Serie (2025)/Season 01/Serie 1x01.mkv");
        mover_seguro(&origen, &destino).unwrap();

        assert_eq!(modo(&destino), 0o640, "0o750 & 0o666");
        assert!(!origen.exists(), "el origen se borra tras copiar");
        assert!(
            !destino.with_file_name("Serie 1x01.mkv.part").exists(),
            "no debe quedar el temporal"
        );
        let _ = fs::remove_dir_all(&origen_dir);
        let _ = fs::remove_dir_all(&raiz);
    }

    /// Cuando somos nosotros quienes creamos la carpeta de la serie, copia los
    /// permisos del nivel anterior en vez de aplicar la umask del proceso. Los
    /// niveles que ya existían no se tocan.
    #[test]
    fn crear_dirs_copia_el_modo_del_nivel_anterior() {
        let raiz = caja("dirs");
        fs::set_permissions(&raiz, fs::Permissions::from_mode(0o701)).unwrap();

        let hoja = raiz.join("Serie (2025)/Season 01");
        crear_dirs(&hoja).unwrap();

        assert_eq!(modo(&raiz), 0o701, "un directorio preexistente no se toca");
        assert_eq!(modo(&raiz.join("Serie (2025)")), 0o701);
        assert_eq!(modo(&hoja), 0o701);
        let _ = fs::remove_dir_all(&raiz);
    }

    /// El caso de la biblioteca abierta del NAS: raíz en `777` y todo lo que
    /// cuelga de ella igual, que es lo que arregla el `Permission denied` de
    /// Jellyfin por NFS.
    #[test]
    fn crear_dirs_propaga_una_biblioteca_abierta() {
        let raiz = caja("dirs-777");
        fs::set_permissions(&raiz, fs::Permissions::from_mode(0o777)).unwrap();

        let hoja = raiz.join("Pluribus (2025)/Season 01");
        crear_dirs(&hoja).unwrap();

        assert_eq!(modo(&raiz.join("Pluribus (2025)")), 0o777);
        assert_eq!(modo(&hoja), 0o777);
        let _ = fs::remove_dir_all(&raiz);
    }
}
