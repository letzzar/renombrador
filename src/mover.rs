//! Operaciones de sistema de archivos seguras.

use std::fs;
use std::path::Path;

/// Ajuste de permisos y propietario del resultado final. Solo tiene efecto en
/// Unix. Modo, dueño y grupo van siempre juntos: aquí "permisos" son los tres.
///
/// **Manda el origen; el destino solo corrige lo que choca.** El archivo venía
/// de una carpeta de descargas que el usuario ya usa, así que sus permisos son
/// casi siempre los buenos, y renombrarlo no es motivo para cambiárselos. Lo
/// que se corrige es solo lo que la biblioteca no aceptaría:
///
/// - **Dueño y grupo**: si la biblioteca tiene dueño conocido, manda ella —su
///   contenido es de quien la posee—. Si no se pudo leer, se conservan los del
///   origen.
/// - **Modo**: se conserva el del origen, ensanchado hasta lo que la biblioteca
///   deja a grupo y otros. Nunca se recorta: un archivo más abierto que su
///   carpeta no rompe nada (la carpeta ya decide quién entra), pero uno más
///   cerrado deja de reproducirse por NFS o desde otro contenedor.
///
/// Hubo un intento anterior de tomarlo **todo** del destino y no mirar el
/// origen, por un `000` que se atribuyó a los archivos de descargas. El `000`
/// era del destino: una `_revisar` con ACL de Synology, donde los bits POSIX se
/// leen a cero. Ignorar el origen no arreglaba aquello y sí perdía lo que sí
/// estaba bien — en la copia entre montajes, el dueño se perdía entero y todo
/// acababa en `root`.
///
/// `FILE_MODE`/`DIR_MODE`/`PUID`/`PGID` sobreescriben cualquiera de las dos
/// mitades.
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

    /// Modo, propietario y **dispositivo** de `ruta`, si se puede leer su
    /// metadata. El dispositivo delata dónde acaba el montaje de la
    /// biblioteca: ver `Herencia::del_destino`.
    pub fn estado_de(ruta: &Path) -> Option<(u32, u32, u32, u64)> {
        fs::metadata(ruta)
            .ok()
            .map(|m| (m.mode() & 0o7777, m.uid(), m.gid(), m.dev()))
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

/// Sube por los ancestros de `destino` buscando el nivel que sirve de
/// referencia: el primero que existe, con algún bit de lectura y **dentro del
/// mismo sistema de archivos** que el primer nivel existente.
///
/// `estado` se inyecta —igual que en los desempates de `organizer`— para poder
/// probar el límite de montaje sin montar nada: en un test no hay forma de
/// fabricar un padre en otro sistema de archivos sin ser root.
#[cfg(unix)]
fn referencia_para<F>(destino: &Path, mut estado: F) -> Option<(u32, u32, u32, u64)>
where
    F: FnMut(&Path) -> Option<(u32, u32, u32, u64)>,
{
    let mut referencia = Some(destino);
    // Dispositivo del primer nivel que existe: el de la biblioteca.
    let mut dispositivo: Option<u64> = None;
    loop {
        let r = referencia?;
        if r.as_os_str().is_empty() {
            return None;
        }
        match estado(r) {
            Some(e) => {
                if *dispositivo.get_or_insert(e.3) != e.3 {
                    // Hemos salido del montaje: ahí fuera ya no hay nada que
                    // podamos llamar "los permisos de la biblioteca".
                    return None;
                }
                if e.0 & 0o444 != 0 {
                    return Some(e);
                }
                // Existe pero no sirve de referencia: seguir subiendo.
                referencia = r.parent();
            }
            // No existe todavía: es un nivel que vamos a crear nosotros.
            None => referencia = r.parent(),
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
    /// `FILE_MODE`: si está puesto, es el modo final y no se mira el origen.
    #[cfg(unix)]
    modo_archivo_forzado: Option<u32>,
    /// Lo que la biblioteca deja como mínimo a grupo y otros. No sustituye al
    /// modo del origen: lo ensancha.
    #[cfg(unix)]
    modo_archivo_minimo: Option<u32>,
    #[cfg(unix)]
    modo_dir: Option<u32>,
    #[cfg(unix)]
    uid: Option<u32>,
    #[cfg(unix)]
    gid: Option<u32>,
}

/// Lo que el archivo de origen trae puesto y hay que conservarle. Vacío en
/// plataformas sin permisos POSIX.
#[derive(Clone, Copy, Default)]
pub struct Origen {
    #[cfg(unix)]
    modo: Option<u32>,
    #[cfg(unix)]
    uid: Option<u32>,
    #[cfg(unix)]
    gid: Option<u32>,
}

impl Origen {
    /// Se lee **antes** de mover nada: en cuanto el archivo cambia de sitio (o
    /// se copia, que lo deja del dueño del proceso) ya no se puede preguntar.
    #[cfg(unix)]
    pub fn de(ruta: &Path) -> Self {
        match permisos::estado_de(ruta) {
            Some((m, u, g, _)) => Self {
                modo: Some(m),
                uid: Some(u),
                gid: Some(g),
            },
            None => Self::default(),
        }
    }

    #[cfg(not(unix))]
    pub fn de(_ruta: &Path) -> Self {
        Self::default()
    }
}

impl Herencia {
    /// Toma como referencia el primer ancestro de `destino` que ya existe
    /// (`destino` mismo si existe): la carpeta de la biblioteca en la que
    /// aterriza el archivo.
    ///
    /// - Los directorios nuevos copian su modo tal cual, bits setgid incluidos
    ///   (un `2775` en la raíz de la biblioteca debe seguir propagándose). Los
    ///   creamos nosotros, así que no hay origen que conservarles.
    /// - De los archivos solo sale el **mínimo** de lectura y escritura
    ///   (`& 0o666`), que se suma al modo del origen: un `.mkv` no es
    ///   ejecutable, y setuid/setgid en un archivo significan algo muy distinto
    ///   que en una carpeta.
    /// - `uid`/`gid` salen de esa misma carpeta y **sustituyen** a los del
    ///   origen: el contenido de una biblioteca es de quien la posee.
    ///
    /// Un nivel **sin ningún bit de lectura** (`000`, `--x--x--x`) no se acepta
    /// como referencia y se sigue subiendo. No es una carpeta "muy cerrada" de
    /// la que haya que aprender: en un recurso con ACL —lo normal en un NAS
    /// Synology, se ve por el `+` de `ls -l`— los permisos de verdad viven en
    /// la ACL y los bits POSIX se quedan a cero. Copiar ese cero a un archivo
    /// lo deja ilegible de verdad, que es justo el `000` que veíamos: una
    /// `_revisar` heredada de una versión antigua envenenaba todo lo que caía
    /// dentro. Su `uid:gid` tampoco vale (aparecía como `letzzar:root`), así
    /// que se descarta el nivel entero, no solo el modo.
    ///
    /// Esa subida **no sale nunca del montaje de la biblioteca**, y el tope es
    /// el número de dispositivo del primer nivel que existe. Sin ese tope, un
    /// `/series` ilegible llevaba la referencia hasta `/`, la raíz del propio
    /// contenedor, que es `root:root 755`: de ahí salió `Dalgliesh (2021)` en
    /// `drwxr-xr-x root root` con sus seis capítulos en `-rw-r--r-- root root`,
    /// en una biblioteca donde todo lo demás es `letzzar:users`. Y encima ese
    /// `chmod` se lleva por delante la ACL que el recurso propaga solo (el `+`
    /// de `ls -l`), que es donde vivía el acceso de verdad.
    ///
    /// Sin referencia válida dentro del montaje no se aplica nada: es mejor
    /// dejar que la ACL del recurso haga su trabajo que imponer los permisos de
    /// un sistema de archivos que no tiene nada que ver. Para fijarlos a mano
    /// están `PUID`/`PGID`/`FILE_MODE`/`DIR_MODE`, que van por delante de todo
    /// esto.
    #[cfg(unix)]
    pub fn del_destino(destino: &Path) -> Self {
        let (modo_archivo_env, modo_dir_env) = permisos::modos_forzados();
        let (uid_env, gid_env) = permisos::ids_forzados();

        let (modo, uid, gid) = match referencia_para(destino, permisos::estado_de) {
            Some((m, u, g, _)) => (Some(m), Some(u), Some(g)),
            None => (None, None, None),
        };

        Self {
            modo_archivo_forzado: modo_archivo_env,
            modo_archivo_minimo: modo.map(|m| m & 0o666),
            modo_dir: modo_dir_env.or(modo),
            uid: uid_env.or(uid),
            gid: gid_env.or(gid),
        }
    }

    #[cfg(not(unix))]
    pub fn del_destino(_destino: &Path) -> Self {
        Self::default()
    }

    /// Modo, dueño y grupo con los que se deposita un archivo concreto: los
    /// suyos de origen, corregidos donde chocan con la biblioteca.
    ///
    /// El modo se **ensancha**, nunca se recorta. El dueño y el grupo sí se
    /// sustituyen cuando la biblioteca los tiene: un archivo que conserva su
    /// dueño de descargas dentro de una biblioteca ajena es justo lo que deja
    /// una carpeta llena de dueños distintos.
    ///
    /// `None` en cualquiera de los tres significa "no tocar eso", que es
    /// preferible a inventarse un valor: sin origen legible ni referencia
    /// válida, lo que haya puesto el sistema de archivos (o la ACL del recurso)
    /// se queda como está.
    #[cfg(unix)]
    fn para_archivo(&self, origen: Origen) -> (Option<u32>, Option<u32>, Option<u32>) {
        let modo = match self.modo_archivo_forzado {
            Some(forzado) => Some(forzado),
            None => match (origen.modo.map(|m| m & 0o666), self.modo_archivo_minimo) {
                (Some(o), Some(m)) => Some(o | m),
                (Some(o), None) => Some(o),
                (None, m) => m,
            },
        };
        (modo, self.uid.or(origen.uid), self.gid.or(origen.gid))
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

/// `uid:gid` con el que este proceso deja lo que crea.
///
/// La librería estándar no expone `getuid`, así que se le pregunta al sistema
/// de archivos: se crea un archivo en el temporal y se mira de quién sale. Es
/// además la respuesta que importa —la que van a tener los archivos que
/// depositemos— y no la que diría una llamada al sistema.
#[cfg(unix)]
pub fn identidad_efectiva() -> Option<(u32, u32)> {
    use std::os::unix::fs::MetadataExt;
    let sonda = std::env::temp_dir().join(format!("renombrador-id-{}", std::process::id()));
    let meta = fs::File::create(&sonda)
        .ok()
        .and_then(|_| fs::metadata(&sonda).ok());
    let _ = fs::remove_file(&sonda);
    meta.map(|m| (m.uid(), m.gid()))
}

#[cfg(not(unix))]
pub fn identidad_efectiva() -> Option<(u32, u32)> {
    None
}

/// ¿Se puede escribir de verdad en `dir`?
///
/// Mirar los bits de permiso no responde a esta pregunta en un NAS: con una ACL
/// de por medio dicen una cosa y el kernel decide otra. Se comprueba creando y
/// borrando un archivo, que es lo único que no miente.
pub fn se_puede_escribir(dir: &Path) -> bool {
    let sonda = dir.join(format!(".renombrador-escritura-{}", std::process::id()));
    match fs::File::create(&sonda) {
        Ok(_) => {
            let _ = fs::remove_file(&sonda);
            true
        }
        Err(_) => false,
    }
}

/// Deja el archivo con lo que traía de origen, corregido donde choca con el
/// directorio de destino.
#[cfg(unix)]
fn normalizar_archivo(ruta: &Path, h: Herencia, origen: Origen) {
    let (modo, uid, gid) = h.para_archivo(origen);
    permisos::aplicar(ruta, modo, uid, gid);
}

#[cfg(not(unix))]
fn normalizar_archivo(_ruta: &Path, _h: Herencia, _origen: Origen) {}

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
    // Y el origen se lee antes de tocarlo: después de un `rename` ya no está
    // ahí, y una `copy` deja el resultado del dueño del proceso (nosotros,
    // `root`) sin dejar rastro de quién era.
    let origen_previo = Origen::de(origen);

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
        normalizar_archivo(destino, herencia, origen_previo);
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
    normalizar_archivo(&tmp, herencia, origen_previo);
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

    /// Al crear `_revisar` dentro de descargas, hereda el modo y el `uid:gid`
    /// de la carpeta que la contiene, no la umask del proceso.
    #[test]
    fn crear_revisar_hereda_de_la_carpeta_que_la_contiene() {
        let raiz = caja("revisar");
        let descargas = raiz.join("descargas");
        fs::create_dir_all(&descargas).unwrap();
        fs::set_permissions(&descargas, fs::Permissions::from_mode(0o777)).unwrap();
        let esperado = duenio(&descargas);

        let revisar = descargas.join("_revisar");
        crear_dirs(&revisar).unwrap();

        assert_eq!(modo(&revisar), 0o777, "el modo sale de descargas");
        assert_eq!(duenio(&revisar), esperado, "y el dueño y el grupo también");
        let _ = fs::remove_dir_all(&raiz);
    }

    /// El bug real: una `_revisar` que ya existía en `000` —el aspecto que
    /// tiene una carpeta con ACL en un NAS Synology— se usaba como referencia
    /// y dejaba en `000` todo lo que iba a cuarentena. Un nivel sin ningún bit
    /// de lectura no es referencia válida: se sigue subiendo.
    #[test]
    fn una_carpeta_de_destino_en_000_no_envenena_lo_que_cae_dentro() {
        let raiz = caja("revisar-roto");
        let descargas = raiz.join("descargas");
        fs::create_dir_all(&descargas).unwrap();
        fs::set_permissions(&descargas, fs::Permissions::from_mode(0o775)).unwrap();

        // La `_revisar` heredada de una versión antigua, ilegible.
        let revisar = descargas.join("_revisar");
        fs::create_dir_all(&revisar).unwrap();
        fs::set_permissions(&revisar, fs::Permissions::from_mode(0o000)).unwrap();

        // Se comprueba la herencia calculada y no un `mover_seguro` completo
        // porque el test no corre como root y no podría ni escribir dentro de
        // una carpeta `000`. El contenedor sí es root: escribe, y por eso el
        // archivo llegaba a existir con el `000` heredado.
        let esperado = duenio(&descargas);
        let h = Herencia::del_destino(&revisar.join("descarga.mkv"));

        assert_eq!(
            h.modo_archivo_minimo,
            Some(0o664),
            "el mínimo sale de descargas (0o775 & 0o666), no del 000 de _revisar"
        );
        assert_eq!(h.modo_dir, Some(0o775));
        assert_eq!(
            (h.uid, h.gid),
            (Some(esperado.0), Some(esperado.1)),
            "el uid:gid también sale de descargas: el de _revisar tampoco vale"
        );
        assert_eq!(
            modo(&revisar),
            0o000,
            "la carpeta preexistente no se toca, solo se ignora como referencia"
        );

        // Hay que poder volver a entrar para limpiar.
        fs::set_permissions(&revisar, fs::Permissions::from_mode(0o755)).unwrap();
        let _ = fs::remove_dir_all(&raiz);
    }

    /// Los dos sistemas de archivos que ve el contenedor: el bind-mount de la
    /// biblioteca y la raíz del propio contenedor, que no tienen nada que ver.
    const DEV_BIBLIOTECA: u64 = 42;
    const DEV_RAIZ_CONTENEDOR: u64 = 1;

    /// El árbol real del contenedor el 03-09-2026: `/series` es el recurso del
    /// NAS, con ACL, y por eso sus bits POSIX están a cero; `/` es la raíz del
    /// contenedor, en otro sistema de archivos y con otro dueño.
    fn arbol_del_contenedor(p: &Path) -> Option<(u32, u32, u32, u64)> {
        match p.to_str().unwrap() {
            "/series" => Some((0o000, 1026, 100, DEV_BIBLIOTECA)),
            "/" => Some((0o755, 0, 0, DEV_RAIZ_CONTENEDOR)),
            // Todo lo que cuelga de /series aún no existe: lo creamos nosotros.
            _ => None,
        }
    }

    /// El bug del 03-09-2026: `/series` no vale de referencia (ACL, bits a
    /// cero) y la subida seguía hasta `/`, la raíz del contenedor, de donde
    /// salieron `Dalgliesh (2021)` y sus seis capítulos en `root:root`. La
    /// biblioteca acaba en su montaje: fuera no se aprende nada.
    #[test]
    fn la_referencia_no_sale_del_montaje_de_la_biblioteca() {
        let destino = Path::new("/series/Dalgliesh (2021)/Season 01/Dalgliesh 1x01.mkv");
        assert_eq!(referencia_para(destino, arbol_del_contenedor), None);
    }

    /// Y sin referencia no se toca nada: es preferible dejar que la ACL del
    /// recurso propague lo suyo a imponer el `root:root 755` de la raíz del
    /// contenedor (que además borra la ACL al hacer el chmod).
    /// Sin biblioteca de la que aprender, el archivo se queda tal y como venía:
    /// no se le impone nada, que es lo contrario de lo que pasaba cuando la
    /// referencia se escapaba a la raíz del contenedor.
    #[test]
    fn sin_referencia_valida_el_archivo_conserva_lo_suyo() {
        let sin_biblioteca = Herencia::default();
        let raiz = caja("sin-referencia");
        let f = raiz.join("x.mkv");
        fs::write(&f, b"video").unwrap();
        fs::set_permissions(&f, fs::Permissions::from_mode(0o604)).unwrap();
        let antes = duenio(&f);
        let origen = Origen::de(&f);

        normalizar_archivo(&f, sin_biblioteca, origen);

        assert_eq!(modo(&f), 0o604, "el modo del origen, intacto");
        assert_eq!(duenio(&f), antes, "y su dueño y grupo también");
        let _ = fs::remove_dir_all(&raiz);
    }

    /// El corazón del modelo: se parte de lo que traía el archivo y la
    /// biblioteca solo corrige lo que choca. El modo se ensancha hasta lo que
    /// ella deja, nunca se recorta; el dueño y el grupo sí los sustituye.
    #[test]
    fn el_origen_manda_y_la_biblioteca_solo_ensancha() {
        let biblioteca = Herencia {
            modo_archivo_forzado: None,
            modo_archivo_minimo: Some(0o664),
            modo_dir: Some(0o775),
            uid: Some(1026),
            gid: Some(100),
        };

        // Un archivo más cerrado que la biblioteca: se abre hasta ella.
        let cerrado = Origen {
            modo: Some(0o600),
            uid: Some(0),
            gid: Some(0),
        };
        assert_eq!(
            biblioteca.para_archivo(cerrado),
            (Some(0o664), Some(1026), Some(100))
        );

        // Uno más abierto: se respeta, no se recorta.
        let abierto = Origen {
            modo: Some(0o666),
            uid: Some(1026),
            gid: Some(100),
        };
        assert_eq!(
            biblioteca.para_archivo(abierto),
            (Some(0o666), Some(1026), Some(100))
        );

        // Y sin dueño conocido en la biblioteca, el del origen se conserva:
        // es justo lo que la copia entre montajes perdía.
        let sin_duenio = Herencia {
            uid: None,
            gid: None,
            ..biblioteca
        };
        assert_eq!(
            sin_duenio.para_archivo(abierto),
            (Some(0o666), Some(1026), Some(100))
        );
    }

    /// `FILE_MODE` es un modo final, no un mínimo: cuando está puesto, ni el
    /// origen ni la biblioteca opinan.
    #[test]
    fn el_modo_forzado_no_se_mezcla_con_el_del_origen() {
        let forzado = Herencia {
            modo_archivo_forzado: Some(0o640),
            modo_archivo_minimo: Some(0o666),
            modo_dir: None,
            uid: None,
            gid: None,
        };
        let origen = Origen {
            modo: Some(0o666),
            uid: Some(7),
            gid: Some(7),
        };
        assert_eq!(
            forzado.para_archivo(origen),
            (Some(0o640), Some(7), Some(7))
        );
    }

    /// El nivel bueno sigue ganando cuando lo hay: si `/series` fuese legible,
    /// es de él de quien se hereda y no se sube más.
    #[test]
    fn dentro_del_montaje_la_referencia_es_la_biblioteca() {
        fn arbol(p: &Path) -> Option<(u32, u32, u32, u64)> {
            match p.to_str().unwrap() {
                "/series" => Some((0o777, 1026, 100, DEV_BIBLIOTECA)),
                "/" => Some((0o755, 0, 0, DEV_RAIZ_CONTENEDOR)),
                _ => None,
            }
        }
        let destino = Path::new("/series/Dalgliesh (2021)/Season 01/Dalgliesh 1x01.mkv");
        assert_eq!(
            referencia_para(destino, arbol),
            Some((0o777, 1026, 100, DEV_BIBLIOTECA))
        );
    }

    /// Una carpeta cerrada pero legible por su dueño sigue siendo referencia
    /// válida: solo se descarta la que no deja leer a nadie.
    ///
    /// Y aquí se ve que la biblioteca **ensancha pero no recorta**: la carpeta
    /// es `700`, el archivo llegó en `644` y sale en `644`. No se le quitan los
    /// bits de lectura que ya traía porque la carpeta sea privada: quien no
    /// pueda entrar en un `700` no llega al archivo de todas formas, y recortar
    /// es como se rompen las reproducciones por NFS.
    #[test]
    fn una_carpeta_privada_pero_legible_si_vale_de_referencia() {
        let raiz = caja("privada");
        fs::set_permissions(&raiz, fs::Permissions::from_mode(0o700)).unwrap();

        let origen = std::env::temp_dir().join(format!("origen-priv-{}.mkv", std::process::id()));
        fs::write(&origen, b"video").unwrap();
        fs::set_permissions(&origen, fs::Permissions::from_mode(0o644)).unwrap();

        let destino = raiz.join("x.mkv");
        mover_seguro(&origen, &destino).unwrap();

        assert_eq!(
            modo(&destino),
            0o644,
            "el 644 del origen, no el 600 que daría la carpeta"
        );
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
