//! Operaciones de sistema de archivos seguras.

use std::fs;
use std::path::Path;

/// Ajuste de permisos y propietario del resultado final. Solo tiene efecto en
/// Unix. Modo, dueño y grupo van siempre juntos: aquí "permisos" son los tres.
///
/// **Manda el archivo que se mueve. La carpeta de destino solo cubre lo que él
/// no puede dar.** Sus permisos vienen de una carpeta de descargas que el
/// usuario ya usa, así que casi siempre son los buenos, y renombrar un archivo
/// no es motivo para cambiárselos. Los tres casos, en orden:
///
/// 1. **No se identifica y va a `_revisar`.** Se mueve preservándolo todo: modo,
///    dueño y grupo salen intactos al otro lado.
/// 2. **Se identifica y hay que crear la serie.** Las carpetas nuevas se hacen a
///    medida del archivo que las estrena: su mismo dueño, su mismo grupo y su
///    mismo modo, más el permiso de paso (`x`) donde haya lectura, porque una
///    carpeta sin `x` no se puede ni abrir.
/// 3. **Lo que trae el archivo no cuadra.** Entonces manda la carpeta anterior,
///    porque conservar la estructura del NAS es lo primero: un modo sin ningún
///    bit de lectura no sirve de referencia, y un modo más cerrado que el de la
///    biblioteca se ensancha hasta el suyo (si no, el archivo deja de
///    reproducirse por NFS o desde otro contenedor). Al revés no: un archivo más
///    abierto que su carpeta no rompe nada, porque la carpeta ya decide quién
///    entra. Los bits especiales de la carpeta (setgid, sticky) también son
///    estructura y se conservan siempre.
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
    /// **Lo que ya está bien no se toca.** No es una optimización: en un recurso
    /// con ACL de Synology, un `chmod` reescribe los bits POSIX y se lleva por
    /// delante la ACL heredada (el `+` de `ls -l`), que es donde vive el acceso
    /// de verdad. Un archivo que ya llega con el modo que le toca no necesita
    /// ninguna llamada, y ahorrársela es lo que le conserva la ACL. Con el
    /// `chown` pasa lo mismo y además limpia los bits setuid/setgid.
    ///
    /// Los fallos no son fatales: el archivo ya está en su sitio, y ni el
    /// `chown` (que exige ser root o tener CAP_CHOWN) ni el `chmod` pueden
    /// hacer nada en un montaje que no los soporte.
    pub fn aplicar(ruta: &Path, modo: Option<u32>, uid: Option<u32>, gid: Option<u32>) {
        use std::os::unix::fs::PermissionsExt;
        let antes = estado_de(ruta);

        // El chown va antes que el chmod: cambiar de propietario limpia los
        // bits setuid/setgid, así que al revés desharía un modo tipo 2775.
        let hubo_chown = match antes {
            Some((_, u, g, _)) => uid.is_some_and(|n| n != u) || gid.is_some_and(|n| n != g),
            None => uid.is_some() || gid.is_some(),
        };
        if hubo_chown {
            let _ = std::os::unix::fs::chown(ruta, uid, gid);
        }

        if let Some(modo) = modo {
            // El modo se compara con el de AHORA, no con el de antes del chown.
            // En un recurso con ACL de Synology, cambiar de dueño reescribe los
            // bits POSIX y los deja a cero, así que el estado previo ya no dice
            // nada del archivo que tenemos delante. Mirarlo hacía creer que "ya
            // estaba bien" un `777` que el chown acababa de destruir, y el
            // archivo se quedaba en `000` sin que nadie lo tocara. Las carpetas
            // no lo enseñaban porque necesitaban el chmod igual, para subir del
            // `755` que deja la umask.
            let ahora = if hubo_chown { estado_de(ruta) } else { antes };
            if ahora.map(|a| a.0) != Some(modo) {
                let _ = fs::set_permissions(ruta, fs::Permissions::from_mode(modo));
            }
        }
    }

    /// `modo uid:gid` en el formato del log, con `-` en lo que no se sepa.
    pub fn describir(modo: Option<u32>, uid: Option<u32>, gid: Option<u32>) -> String {
        let n = |v: Option<u32>| v.map(|v| v.to_string()).unwrap_or_else(|| "-".to_string());
        format!(
            "{} {}:{}",
            modo.map(|m| format!("{:o}", m))
                .unwrap_or_else(|| "-".to_string()),
            n(uid),
            n(gid)
        )
    }

    /// Comprueba el resultado y describe la diferencia si no es el que se pidió.
    /// `None` cuando quedó como debía.
    ///
    /// Devuelve el texto en vez de imprimirlo para poder probarlo: el caso que
    /// tiene que cazar solo se da con root sobre un recurso con ACL, y sin esto
    /// no habría forma de fijar cuándo salta.
    pub fn discrepancia(
        ruta: &Path,
        modo: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
    ) -> Option<String> {
        let (m, u, g, _) = estado_de(ruta)?;
        let modo_mal = modo.is_some_and(|q| q != m);
        let duenio_mal = uid.is_some_and(|q| q != u) || gid.is_some_and(|q| q != g);
        if !modo_mal && !duenio_mal {
            return None;
        }
        Some(format!(
            "'{}' quedó en {} y se le pidió {} (¿ACL del recurso rechazando el cambio?)",
            ruta.display(),
            describir(Some(m), Some(u), Some(g)),
            describir(modo, uid, gid),
        ))
    }
}

/// Qué se ha podido averiguar del sitio al que va el archivo.
#[cfg(unix)]
#[derive(Clone, Copy, PartialEq, Debug)]
enum Referencia {
    /// Un nivel del que aprender: modo, `uid` y `gid`.
    Valida(u32, u32, u32),
    /// La carpeta existe pero sus bits POSIX se leen a cero. No es una carpeta
    /// cerrada: es un recurso que lleva los permisos por **ACL** —lo normal en
    /// un NAS Synology, se ve por el `+` de `ls -l`— y los bits POSIX son solo
    /// la sombra que proyecta.
    ///
    /// Aquí no hay modo que copiar y, sobre todo, **no hay que imponer
    /// ninguno**: cada `chmod` reescribe esos bits y borra la ACL que el
    /// recurso acababa de heredarle al archivo. Se conserva el `uid:gid` por si
    /// hace falta de respaldo.
    GestionadaPorAcl(u32, u32),
    /// Nada de lo que aprender: ni existe, ni está en este montaje.
    Ninguna,
}

/// Sube por los ancestros de `destino` buscando el nivel que sirve de
/// referencia: el primero que existe, con algún bit de lectura y **dentro del
/// mismo sistema de archivos** que el primer nivel existente.
///
/// Si por el camino solo aparecen niveles ilegibles, eso no es "no se sabe
/// nada": es la firma de un recurso con ACL, y se devuelve como tal.
///
/// `estado` se inyecta —igual que en los desempates de `organizer`— para poder
/// probar el límite de montaje sin montar nada: en un test no hay forma de
/// fabricar un padre en otro sistema de archivos sin ser root.
#[cfg(unix)]
fn referencia_para<F>(destino: &Path, mut estado: F) -> Referencia
where
    F: FnMut(&Path) -> Option<(u32, u32, u32, u64)>,
{
    let mut referencia = Some(destino);
    // Dispositivo del primer nivel que existe: el de la biblioteca.
    let mut dispositivo: Option<u64> = None;
    // Dueño del primer nivel ilegible que veamos, por si al final resulta que
    // todo el camino es un recurso con ACL.
    let mut duenio_acl: Option<(u32, u32)> = None;

    let sin_referencia = |acl: Option<(u32, u32)>| match acl {
        Some((u, g)) => Referencia::GestionadaPorAcl(u, g),
        None => Referencia::Ninguna,
    };

    loop {
        let Some(r) = referencia else {
            return sin_referencia(duenio_acl);
        };
        if r.as_os_str().is_empty() {
            return sin_referencia(duenio_acl);
        }
        match estado(r) {
            Some(e) => {
                if *dispositivo.get_or_insert(e.3) != e.3 {
                    // Hemos salido del montaje: ahí fuera ya no hay nada que
                    // podamos llamar "los permisos de la biblioteca".
                    return sin_referencia(duenio_acl);
                }
                if e.0 & 0o444 != 0 {
                    return Referencia::Valida(e.0, e.1, e.2);
                }
                // Existe pero no sirve de referencia: seguir subiendo,
                // recordando que aquí manda una ACL.
                duenio_acl.get_or_insert((e.1, e.2));
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
    /// `FILE_MODE`: si está puesto, es el modo final del archivo.
    #[cfg(unix)]
    modo_archivo_forzado: Option<u32>,
    /// `DIR_MODE`: si está puesto, es el modo final de las carpetas nuevas.
    #[cfg(unix)]
    modo_dir_forzado: Option<u32>,
    /// Modo de la carpeta de destino, tal cual. Es el caso 3: lo que cubre al
    /// archivo cuando lo suyo no cuadra, y de donde salen siempre los bits
    /// especiales (setgid, sticky) de las carpetas nuevas.
    #[cfg(unix)]
    modo_carpeta: Option<u32>,
    /// `PUID`/`PGID`: ganan al archivo y a la carpeta.
    #[cfg(unix)]
    uid_forzado: Option<u32>,
    #[cfg(unix)]
    gid_forzado: Option<u32>,
    /// Dueño de la carpeta de destino: solo se usa si el archivo no lo da.
    #[cfg(unix)]
    uid_carpeta: Option<u32>,
    #[cfg(unix)]
    gid_carpeta: Option<u32>,
    /// El destino lleva los permisos por ACL: no se impone ningún modo, ni al
    /// archivo ni a las carpetas. Ver [`Referencia::GestionadaPorAcl`].
    #[cfg(unix)]
    la_acl_manda: bool,
}

/// Modo con el que se deposita un archivo cuando no hay de dónde sacarlo: ni el
/// origen sirve (llegó sin un solo bit de lectura) ni la carpeta de destino da
/// referencia.
///
/// Es el último recurso, y existe porque la alternativa —no tocar nada— deja en
/// la biblioteca un archivo que no puede abrir nadie. Pasó de verdad: en el NAS,
/// `/series` es un recurso con ACL y sus bits POSIX se leen a cero, así que no
/// da referencia; con un origen ya en `000`, las dos fuentes fallaban a la vez y
/// el `000` se propagaba sin que nada lo dijera, porque tampoco se pedía ningún
/// modo que pudiera desmentirse después.
///
/// `644` y no algo más abierto porque esto es exactamente el caso en el que no
/// sabemos nada de la biblioteca: deja el archivo legible para todos, que es lo
/// que hacía falta, sin repartir escritura a ciegas. `FILE_MODE` está para
/// cuando la biblioteca quiere otra cosa.
#[cfg(unix)]
const MODO_ULTIMO_RECURSO: u32 = 0o644;

/// Modo de carpeta equivalente a un modo de archivo: el mismo, más permiso de
/// paso (`x`) allí donde hay lectura. Copiarlo tal cual dejaría una carpeta que
/// no se puede ni abrir.
#[cfg(unix)]
fn modo_de_carpeta(modo_archivo: u32) -> u32 {
    let lectura = modo_archivo & 0o444;
    (modo_archivo & 0o777) | (lectura >> 2)
}

/// Lo que el archivo de origen trae puesto y hay que conservarle: modo, dueño,
/// grupo y fechas. Los tres primeros solo existen en plataformas POSIX; las
/// fechas, en todas.
#[derive(Clone, Copy, Default)]
pub struct Origen {
    #[cfg(unix)]
    modo: Option<u32>,
    #[cfg(unix)]
    uid: Option<u32>,
    #[cfg(unix)]
    gid: Option<u32>,
    accedido: Option<std::time::SystemTime>,
    modificado: Option<std::time::SystemTime>,
}

impl Origen {
    /// Se lee **antes** de mover nada: en cuanto el archivo cambia de sitio (o
    /// se copia, que lo deja del dueño del proceso y con la fecha de hoy) ya no
    /// se puede preguntar.
    pub fn de(ruta: &Path) -> Self {
        match fs::metadata(ruta) {
            Ok(meta) => Self::de_metadata(&meta),
            Err(_) => Self::default(),
        }
    }

    #[cfg(unix)]
    fn de_metadata(meta: &fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            modo: Some(meta.mode() & 0o7777),
            uid: Some(meta.uid()),
            gid: Some(meta.gid()),
            accedido: meta.accessed().ok(),
            modificado: meta.modified().ok(),
        }
    }

    #[cfg(not(unix))]
    fn de_metadata(meta: &fs::Metadata) -> Self {
        Self {
            accedido: meta.accessed().ok(),
            modificado: meta.modified().ok(),
        }
    }
}

/// Copia el contenido y **nada más**.
///
/// `fs::copy` hace dos cosas, y la segunda es invisible: además del contenido,
/// le impone al destino el modo del origen. Es un `chmod` que no decidimos
/// nosotros y que llega antes de que nadie haya mirado dónde aterriza el
/// archivo; en un recurso con ACL se lleva por delante la que el destino acaba
/// de heredar, y el archivo pierde el `+` antes de que empecemos.
///
/// Aquí el archivo nuevo se queda con lo que el sistema de archivos le dé —la
/// ACL del recurso, o la umask— y quien decide si hay que cambiarlo, y a qué, es
/// `normalizar_archivo`: una sola vez, y con toda la información delante.
fn copiar_contenido(origen: &Path, destino: &Path) -> std::io::Result<()> {
    let mut o = fs::File::open(origen)?;
    let mut d = fs::File::create(destino)?;
    // `io::copy` entre dos ficheros usa `copy_file_range` en Linux, igual que
    // `fs::copy`: no se paga nada por hacerlo a mano.
    std::io::copy(&mut o, &mut d)?;
    Ok(())
}

/// Copia las fechas del origen al archivo depositado, como hace `cp -p`.
///
/// `fs::copy` trae el contenido y el modo, pero deja la fecha de modificación
/// del destino en "ahora". Un `rename` sí las conserva, así que esto solo tiene
/// efecto en el camino de copia —el que se usa de verdad, porque descargas y
/// biblioteca son montajes distintos— y ahí es donde se perdían: un capítulo
/// bajado hace meses aterrizaba en la biblioteca como recién estrenado y
/// cualquier orden por fecha mentía.
///
/// Un fallo aquí no es fatal: el archivo está en su sitio y con sus permisos, y
/// la fecha es lo único que se queda sin arreglar.
fn preservar_fechas(ruta: &Path, origen: Origen) {
    let (Some(accedido), Some(modificado)) = (origen.accedido, origen.modificado) else {
        return;
    };
    // `set_times` exige el archivo abierto para escritura.
    let Ok(f) = fs::OpenOptions::new().write(true).open(ruta) else {
        return;
    };
    let _ = f.set_times(
        fs::FileTimes::new()
            .set_accessed(accedido)
            .set_modified(modificado),
    );
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

        let (modo, uid, gid, la_acl_manda) = match referencia_para(destino, permisos::estado_de) {
            Referencia::Valida(m, u, g) => (Some(m), Some(u), Some(g), false),
            Referencia::GestionadaPorAcl(u, g) => (None, Some(u), Some(g), true),
            Referencia::Ninguna => (None, None, None, false),
        };

        Self {
            modo_archivo_forzado: modo_archivo_env,
            modo_dir_forzado: modo_dir_env,
            modo_carpeta: modo,
            uid_forzado: uid_env,
            gid_forzado: gid_env,
            uid_carpeta: uid,
            gid_carpeta: gid,
            la_acl_manda,
        }
    }

    #[cfg(not(unix))]
    pub fn del_destino(_destino: &Path) -> Self {
        Self::default()
    }

    /// Modo, dueño y grupo con los que se deposita el archivo: los suyos,
    /// cubiertos por la carpeta solo donde no cuadran (caso 3).
    ///
    /// El modo se **ensancha** hasta el de la carpeta, nunca se recorta. El
    /// dueño y el grupo son los del archivo mientras los tenga; los de la
    /// carpeta son el respaldo, no la norma.
    ///
    /// `None` en cualquiera de los tres significa "no tocar eso", que es
    /// preferible a inventarse un valor: sin origen legible ni referencia
    /// válida, lo que haya puesto el sistema de archivos (o la ACL del recurso)
    /// se queda como está.
    #[cfg(unix)]
    fn para_archivo(&self, origen: Origen) -> (Option<u32>, Option<u32>, Option<u32>) {
        let modo = match self.modo_archivo_forzado {
            Some(forzado) => Some(forzado),
            // El recurso lleva los permisos por ACL: no se impone modo alguno.
            // Cualquiera que pusiéramos sería un `chmod`, y un `chmod` borra la
            // ACL heredada y deja el archivo peor de lo que estaba.
            None if self.la_acl_manda => None,
            // Del archivo se conservan los nueve bits tal cual, `x` incluida:
            // en un recurso del NAS lo normal es que llegue en `777`, y
            // recortarle la `x` obliga a un `chmod` que le costaría la ACL.
            // Lo que no se hereda nunca es setuid/setgid, que en un archivo
            // significan algo muy distinto que en una carpeta.
            //
            // De la carpeta, en cambio, solo se toma lectura y escritura: su
            // `x` es permiso de paso, no de ejecución, y no pinta nada en un
            // `.mkv` que llegó sin ella.
            //
            // Un modo sin ningún bit de lectura no es un permiso que preservar,
            // es un archivo ilegible: ahí solo cuenta la carpeta.
            None => match (
                origen.modo.filter(|m| m & 0o444 != 0).map(|m| m & 0o777),
                self.modo_carpeta.map(|m| m & 0o666),
            ) {
                (Some(o), Some(c)) => Some(o | c),
                (Some(o), None) => Some(o),
                (None, Some(c)) => Some(c),
                // Ninguna de las dos fuentes sirve. No es "no sabemos qué
                // poner", es "el archivo se queda ilegible si no ponemos nada":
                // ver `MODO_ULTIMO_RECURSO`.
                (None, None) => Some(MODO_ULTIMO_RECURSO),
            },
        };
        (
            modo,
            self.uid_forzado.or(origen.uid).or(self.uid_carpeta),
            self.gid_forzado.or(origen.gid).or(self.gid_carpeta),
        )
    }

    /// Lo mismo para una carpeta que estamos creando para ese archivo (caso 2):
    /// se hace a su medida, con paso donde él tiene lectura.
    ///
    /// Los bits especiales salen siempre de la carpeta anterior: un `2775` en la
    /// raíz de una biblioteca es el truco para que todo lo nuevo caiga en el
    /// mismo grupo, y eso es estructura del NAS, no permiso del archivo.
    ///
    /// Sin archivo que copiar —el arranque creando `/series` o `_revisar`— manda
    /// la carpeta anterior tal cual.
    ///
    /// Y si el recurso lleva los permisos por ACL, no se impone ninguno: la
    /// carpeta recién creada ya hereda la del recurso, que es exactamente lo que
    /// se quiere, y un `chmod` encima solo serviría para borrarla.
    #[cfg(unix)]
    fn para_dir(&self, origen: Origen) -> (Option<u32>, Option<u32>, Option<u32>) {
        let especiales = self.modo_carpeta.unwrap_or(0) & 0o7000;
        let modo = match self.modo_dir_forzado {
            Some(forzado) => Some(forzado),
            None if self.la_acl_manda => None,
            None => match origen.modo {
                Some(_) => self
                    .para_archivo(origen)
                    .0
                    .map(|m| modo_de_carpeta(m) | especiales),
                None => self.modo_carpeta,
            },
        };
        let (_, uid, gid) = self.para_archivo(origen);
        (modo, uid, gid)
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

/// Repasa el archivo ya en su nombre definitivo y avisa si no quedó como se
/// pidió.
///
/// Se vuelve a aplicar porque el `rename` final es otro punto donde el recurso
/// puede reinterpretar los permisos, y `aplicar` no cuesta nada cuando ya está
/// todo bien: es justo lo que no toca nada.
#[cfg(unix)]
fn repasar_archivo(ruta: &Path, h: Herencia, origen: Origen) {
    let (modo, uid, gid) = h.para_archivo(origen);
    permisos::aplicar(ruta, modo, uid, gid);

    // Una línea por archivo con las tres cosas que hacen falta para entender
    // cualquier sorpresa de permisos: lo que traía, lo que se le pidió y lo que
    // quedó. Diagnosticar un `000` sin esto costó varios ciclos de prueba
    // enteros, porque cada uno exigía la foto del "antes" tomada a mano y a
    // tiempo, y el "antes" desaparece en cuanto el archivo se mueve.
    println!(
        "[INFO]   -> permisos: origen {} · pedido {} · final {}{}",
        permisos::describir(origen.modo, origen.uid, origen.gid),
        permisos::describir(modo, uid, gid),
        permisos::estado_de(ruta)
            .map(|(m, u, g, _)| permisos::describir(Some(m), Some(u), Some(g)))
            .unwrap_or_else(|| "?".to_string()),
        if h.la_acl_manda {
            "  [recurso con ACL: el modo lo pone él]"
        } else {
            ""
        },
    );
    // El aviso sale por la misma salida que el resto del log —`stdout`— y no
    // por un `Result`: el archivo ya está movido y en su sitio, así que esto no
    // es un fallo de la operación sino el sistema de archivos negándose a lo
    // que le pedimos. Callarlo es lo que costó un ciclo entero de pruebas para
    // ver un `000` en la biblioteca.
    //
    // Por `stdout` y no por `stderr` a propósito: `docker logs` manda cada
    // stream al suyo, así que un aviso en `stderr` se pierde en cuanto alguien
    // filtra el log con `| grep` sin acordarse del `2>&1`. Justo el aviso que
    // más falta hace es el que no puede depender de eso.
    if let Some(aviso) = permisos::discrepancia(ruta, modo, uid, gid) {
        println!("[WARN] {}", aviso);
    }
}

#[cfg(not(unix))]
fn repasar_archivo(_ruta: &Path, _h: Herencia, _origen: Origen) {}

#[cfg(not(unix))]
fn normalizar_archivo(_ruta: &Path, _h: Herencia, _origen: Origen) {}

/// Deja el directorio a medida del archivo que lo estrena, o del nivel del que
/// cuelga si no hay archivo.
#[cfg(unix)]
fn normalizar_dir(ruta: &Path, h: Herencia, origen: Origen) {
    let (modo, uid, gid) = h.para_dir(origen);
    permisos::aplicar(ruta, modo, uid, gid);
}

#[cfg(not(unix))]
fn normalizar_dir(_ruta: &Path, _h: Herencia, _origen: Origen) {}

/// Crea `dir` y sus padres dejando **solo los niveles que no existían** con el
/// modo y el dueño del primer nivel que sí existía. Los directorios ya creados
/// no se tocan, para no alterar permisos que el usuario haya fijado.
pub fn crear_dirs(dir: &Path) -> std::io::Result<()> {
    // Sin archivo que las estrene (el arranque, creando `/series` o
    // `_revisar`): manda el nivel anterior.
    crear_dirs_con(dir, Herencia::del_destino(dir), Origen::default())
}

fn crear_dirs_con(dir: &Path, herencia: Herencia, origen: Origen) -> std::io::Result<()> {
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
        normalizar_dir(d, herencia, origen);
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
        crear_dirs_con(padre, herencia, origen_previo)
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
        repasar_archivo(destino, herencia, origen_previo);
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

    copiar_contenido(origen, &tmp).map_err(|e| {
        // Si la copia falló a medias, intentamos no dejar basura.
        let _ = fs::remove_file(&tmp);
        format!("error al copiar a '{}': {}", tmp.display(), e)
    })?;
    // Ajustamos permisos, propietario y fechas sobre el `.part`, antes del
    // rename final: así el archivo nunca llega a existir con su nombre
    // definitivo y permisos malos, y un escaneo de la biblioteca no puede
    // pillarlo así. Las fechas van primero porque escribir el contenido es lo
    // que las mueve, y ni el chown ni el chmod las tocan.
    preservar_fechas(&tmp, origen_previo);
    normalizar_archivo(&tmp, herencia, origen_previo);
    fs::rename(&tmp, destino).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("error al renombrar '{}' a su nombre final: {}", tmp.display(), e)
    })?;
    // Y se repasa ya con su nombre definitivo: el `rename` es otro punto donde
    // el recurso puede reinterpretar los permisos, y si algo no cuadró, esto es
    // lo que lo dice en el log en vez de dejarlo callado en la biblioteca.
    repasar_archivo(destino, herencia, origen_previo);
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

    fn modificado(p: &Path) -> std::time::SystemTime {
        fs::metadata(p).unwrap().modified().unwrap()
    }

    /// Le pone al archivo una fecha vieja y concreta, para poder reconocerla al
    /// otro lado.
    fn envejecer(p: &Path, dias: u64) -> std::time::SystemTime {
        let cuando = std::time::SystemTime::UNIX_EPOCH
            + std::time::Duration::from_secs(1_700_000_000 - dias * 86_400);
        let f = fs::OpenOptions::new().write(true).open(p).unwrap();
        f.set_times(
            fs::FileTimes::new()
                .set_accessed(cuando)
                .set_modified(cuando),
        )
        .unwrap();
        cuando
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

    /// El dueño y el grupo del archivo llegan al destino, y con ellos salen
    /// también las carpetas que se crean para él.
    ///
    /// El test no corre como root, así que no puede fabricar un origen y una
    /// biblioteca de dueños distintos ni ejecutar un `chown` de verdad; lo que
    /// fija es que el `uid:gid` se lee del **origen** y se aplica a los dos
    /// lados. Que el archivo mandaba sobre la carpeta se comprueba aparte, en
    /// `el_archivo_manda_sobre_la_carpeta`, donde los dueños sí difieren.
    #[test]
    fn el_duenio_del_archivo_llega_al_destino_y_a_sus_carpetas() {
        let raiz = caja("duenio");
        let origen = raiz.join("origen.mkv");
        fs::write(&origen, b"video").unwrap();
        let esperado = duenio(&origen);

        let biblioteca = raiz.join("series");
        fs::create_dir_all(&biblioteca).unwrap();

        let destino = biblioteca.join("Serie (2025)/Season 01/Serie 1x01.mkv");
        mover_seguro(&origen, &destino).unwrap();

        assert_eq!(duenio(&destino), esperado, "el dueño del archivo");
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
            h.modo_carpeta,
            Some(0o775),
            "la referencia es descargas, no el 000 de _revisar"
        );
        assert_eq!(
            (h.uid_carpeta, h.gid_carpeta),
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
    ///
    /// Y lo que se encuentra por el camino no es "nada": es un recurso que lleva
    /// los permisos por ACL, con su dueño, y eso cambia qué hay que hacer.
    #[test]
    fn la_referencia_no_sale_del_montaje_de_la_biblioteca() {
        let destino = Path::new("/series/Dalgliesh (2021)/Season 01/Dalgliesh 1x01.mkv");
        assert_eq!(
            referencia_para(destino, arbol_del_contenedor),
            Referencia::GestionadaPorAcl(1026, 100)
        );
    }

    /// En un recurso con ACL no se impone ningún modo, ni al archivo ni a las
    /// carpetas: la ACL heredada ya hace el trabajo y cada `chmod` la borra.
    /// El dueño sí se arregla, que es lo que la copia deja en `root`.
    #[test]
    fn en_un_recurso_con_acl_no_se_impone_ningun_modo() {
        let destino = Path::new("/series/Dalgliesh (2021)/Season 01/Dalgliesh 1x01.mkv");
        let Referencia::GestionadaPorAcl(u, g) = referencia_para(destino, arbol_del_contenedor)
        else {
            panic!("el árbol del contenedor es justo ese caso");
        };
        let acl = Herencia {
            uid_carpeta: Some(u),
            gid_carpeta: Some(g),
            la_acl_manda: true,
            ..Herencia::default()
        };
        let descargado = archivo(0o777, 1026, 100);

        assert_eq!(
            acl.para_archivo(descargado),
            (None, Some(1026), Some(100)),
            "sin modo que imponer; el dueño, el del archivo"
        );
        assert_eq!(
            acl.para_dir(descargado),
            (None, Some(1026), Some(100)),
            "las carpetas nuevas heredan la ACL del recurso y no se tocan"
        );

        // Ni siquiera un origen ilegible dispara el último recurso: aquí un
        // modo impuesto sería peor que no hacer nada.
        assert_eq!(acl.para_archivo(archivo(0o000, 1026, 100)).0, None);

        // `FILE_MODE`/`DIR_MODE` siguen ganando: quien lo pide a mano, manda.
        let forzado = Herencia {
            modo_archivo_forzado: Some(0o664),
            modo_dir_forzado: Some(0o775),
            ..acl
        };
        assert_eq!(forzado.para_archivo(descargado).0, Some(0o664));
        assert_eq!(forzado.para_dir(descargado).0, Some(0o775));
    }

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

    /// Una biblioteca del NAS: carpeta `2775` de `letzzar:users`.
    fn biblioteca() -> Herencia {
        Herencia {
            modo_archivo_forzado: None,
            modo_dir_forzado: None,
            modo_carpeta: Some(0o2775),
            uid_forzado: None,
            gid_forzado: None,
            uid_carpeta: Some(1026),
            gid_carpeta: Some(100),
            la_acl_manda: false,
        }
    }

    fn archivo(modo: u32, uid: u32, gid: u32) -> Origen {
        Origen {
            modo: Some(modo),
            uid: Some(uid),
            gid: Some(gid),
            ..Origen::default()
        }
    }

    /// Caso 2, el corazón del modelo: manda el archivo. Su dueño y su grupo se
    /// conservan aunque la biblioteca tenga otros, y las carpetas que se crean
    /// para él salen a su medida.
    #[test]
    fn el_archivo_manda_sobre_la_carpeta() {
        let suyo = archivo(0o664, 1030, 65534);
        assert_eq!(
            biblioteca().para_archivo(suyo),
            (Some(0o664), Some(1030), Some(65534)),
            "el dueño del archivo, no el de la biblioteca"
        );
        assert_eq!(
            biblioteca().para_dir(suyo),
            (Some(0o2775), Some(1030), Some(65534)),
            "la carpeta nueva, a su medida: 664 + paso, y el setgid de la biblioteca"
        );
    }

    /// El caso real del NAS: Download Station deja el archivo en `777
    /// letzzar:users` con la ACL del recurso, y la biblioteca es igual. Ahí no
    /// hay nada que cambiar, y **no cambiar nada es el objetivo**: cada chmod
    /// de más se lleva la ACL heredada.
    #[test]
    fn un_archivo_que_ya_esta_bien_no_necesita_ningun_cambio() {
        let recurso = Herencia {
            modo_carpeta: Some(0o777),
            uid_carpeta: Some(1026),
            gid_carpeta: Some(100),
            ..Herencia::default()
        };
        let descargado = archivo(0o777, 1026, 100);

        assert_eq!(
            recurso.para_archivo(descargado),
            (Some(0o777), Some(1026), Some(100)),
            "el 777 del origen se conserva entero, x incluida"
        );
        assert_eq!(
            recurso.para_dir(descargado),
            (Some(0o777), Some(1026), Some(100)),
            "y las carpetas nuevas salen igual que las vecinas"
        );

        // Y sobre el disco, ni un chmod ni un chown: lo que ya está bien no se
        // toca, que es lo único que le conserva la ACL al archivo.
        let raiz = caja("ya-esta-bien");
        let f = raiz.join("x.mkv");
        fs::write(&f, b"video").unwrap();
        fs::set_permissions(&f, fs::Permissions::from_mode(0o777)).unwrap();
        let antes = fs::metadata(&f).unwrap().ctime_nsec();

        let (m, u, g) = (Some(0o777), None, None);
        permisos::aplicar(&f, m, u, g);

        assert_eq!(modo(&f), 0o777);
        assert_eq!(
            fs::metadata(&f).unwrap().ctime_nsec(),
            antes,
            "ni siquiera se ha tocado el inodo: no hubo chmod"
        );
        let _ = fs::remove_dir_all(&raiz);
    }

    /// Lo que hay que cambiar se cambia, aunque lo de al lado ya estuviera bien.
    ///
    /// El fallo del 03-09-2026 vivía justo aquí: `aplicar` decidía el `chmod`
    /// mirando el estado leído ANTES del `chown`, y en el NAS ese `chown` deja
    /// los bits POSIX a cero. El archivo llegaba con el modo correcto, el chown
    /// se lo borraba y la comparación con el estado viejo decía "ya está bien".
    ///
    /// El caso completo necesita root y un recurso con ACL, así que aquí solo se
    /// fija la mitad comprobable: cuando el modo no es el pedido, se aplica.
    #[test]
    fn un_modo_distinto_del_pedido_si_se_aplica() {
        let raiz = caja("modo-distinto");
        let f = raiz.join("x.mkv");
        fs::write(&f, b"video").unwrap();
        fs::set_permissions(&f, fs::Permissions::from_mode(0o600)).unwrap();

        permisos::aplicar(&f, Some(0o664), None, None);

        assert_eq!(modo(&f), 0o664);
        let _ = fs::remove_dir_all(&raiz);
    }

    /// Y si aun así no queda como se pidió, se dice. Un `000` en la biblioteca
    /// costó un ciclo entero de pruebas por no aparecer en ningún log.
    #[test]
    fn un_resultado_que_no_cuadra_se_describe() {
        let raiz = caja("no-cuadra");
        let f = raiz.join("x.mkv");
        fs::write(&f, b"video").unwrap();
        fs::set_permissions(&f, fs::Permissions::from_mode(0o000)).unwrap();
        let (uid_real, gid_real) = duenio(&f);

        // El escenario exacto del NAS: se pidió 777 y quedó en 000.
        let aviso = permisos::discrepancia(&f, Some(0o777), Some(uid_real), Some(gid_real))
            .expect("un 000 donde se pedía 777 tiene que saltar");
        assert!(aviso.contains("quedó en 0 "), "{aviso}");
        assert!(aviso.contains("se le pidió 777 "), "{aviso}");

        // Y cuando sí cuadra, no se dice nada.
        fs::set_permissions(&f, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(
            permisos::discrepancia(&f, Some(0o777), Some(uid_real), Some(gid_real)).is_none()
        );

        let _ = fs::remove_dir_all(&raiz);
    }

    /// Mover es `cp -p`, no `cp`: la fecha del archivo llega al destino.
    ///
    /// Se prueba cruzando de sistema de archivos, que es el camino que se usa de
    /// verdad (descargas y biblioteca son montajes distintos) y el único donde
    /// se perdía: un `rename` conserva las fechas solo, pero una copia deja el
    /// destino con la de hoy y el capítulo aparece en la biblioteca como recién
    /// estrenado.
    #[test]
    fn mover_entre_sistemas_de_archivos_conserva_la_fecha() {
        let otro_fs = Path::new("/dev/shm");
        if !otro_fs.is_dir() {
            return;
        }
        let origen_dir = otro_fs.join(format!("renombrador-test-fecha-{}", std::process::id()));
        let _ = fs::remove_dir_all(&origen_dir);
        if fs::create_dir_all(&origen_dir).is_err() {
            return;
        }
        let raiz = caja("fecha");

        let origen = origen_dir.join("origen.mkv");
        fs::write(&origen, b"video").unwrap();
        let esperada = envejecer(&origen, 90);

        // Si las dos rutas están en el mismo sistema de archivos, esto iría por
        // `rename` y no probaría el camino de copia.
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

        let destino = raiz.join("series/Serie (2025)/Season 01/Serie 1x01.mkv");
        mover_seguro(&origen, &destino).unwrap();

        assert_eq!(
            modificado(&destino),
            esperada,
            "la fecha del origen, no la del movimiento"
        );
        let _ = fs::remove_dir_all(&origen_dir);
        let _ = fs::remove_dir_all(&raiz);
    }

    /// El caso que dejaba el `000` en la biblioteca: el archivo llega ilegible
    /// **y** la carpeta de destino no da referencia (`/series` es un recurso con
    /// ACL y sus bits POSIX se leen a cero). Con las dos fuentes fuera, "no
    /// tocar nada" significaba dejar en la biblioteca un archivo que no abre
    /// nadie, y encima sin pedir ningún modo, así que ni el aviso saltaba.
    #[test]
    fn un_origen_ilegible_sin_referencia_no_se_queda_en_000() {
        let sin_referencia = Herencia::default();
        let (modo, uid, gid) = sin_referencia.para_archivo(archivo(0o000, 1026, 100));

        assert_eq!(modo, Some(MODO_ULTIMO_RECURSO), "legible para todos");
        assert_ne!(modo, Some(0o000));
        assert_eq!((uid, gid), (Some(1026), Some(100)), "el dueño sí se preserva");
    }

    /// Pero el último recurso es eso, el último: en cuanto una de las dos
    /// fuentes sirve, manda ella y no se inventa nada.
    #[test]
    fn el_ultimo_recurso_no_pisa_a_nadie() {
        // Solo el archivo.
        assert_eq!(
            Herencia::default().para_archivo(archivo(0o600, 1026, 100)).0,
            Some(0o600)
        );
        // Solo la carpeta.
        let solo_carpeta = Herencia {
            modo_carpeta: Some(0o775),
            ..Herencia::default()
        };
        assert_eq!(
            solo_carpeta.para_archivo(archivo(0o000, 1026, 100)).0,
            Some(0o664)
        );
    }

    /// Caso 3: lo que trae el archivo no cuadra y lo cubre la carpeta.
    #[test]
    fn lo_que_no_cuadra_lo_cubre_la_carpeta() {
        // Más cerrado que la biblioteca: se ensancha hasta ella.
        assert_eq!(
            biblioteca().para_archivo(archivo(0o600, 1026, 100)).0,
            Some(0o664)
        );
        // Sin ningún bit de lectura no es un permiso, es un archivo ilegible:
        // no se preserva, manda la carpeta.
        assert_eq!(
            biblioteca().para_archivo(archivo(0o000, 1026, 100)).0,
            Some(0o664)
        );
        // Más abierto: se respeta. La carpeta ya decide quién entra.
        assert_eq!(
            biblioteca().para_archivo(archivo(0o666, 1026, 100)).0,
            Some(0o666)
        );
        // Y sin dueño en el archivo, el de la carpeta es el respaldo.
        let sin_duenio = Origen {
            modo: Some(0o664),
            uid: None,
            gid: None,
            ..Origen::default()
        };
        assert_eq!(
            biblioteca().para_archivo(sin_duenio),
            (Some(0o664), Some(1026), Some(100))
        );
    }

    /// Caso 1: lo que va a `_revisar` sale por el otro lado tal y como entró.
    #[test]
    fn a_revisar_se_mueve_preservandolo_todo() {
        let raiz = caja("revisar-preserva");
        let descargas = raiz.join("descargas");
        fs::create_dir_all(&descargas).unwrap();
        fs::set_permissions(&descargas, fs::Permissions::from_mode(0o777)).unwrap();

        let origen = descargas.join("sin identificar.mkv");
        fs::write(&origen, b"video").unwrap();
        fs::set_permissions(&origen, fs::Permissions::from_mode(0o640)).unwrap();
        let esperado = duenio(&origen);

        let destino = descargas.join("_revisar/sin identificar.mkv");
        mover_seguro(&origen, &destino).unwrap();

        // El 640 se ensancha a 666 porque descargas es 777 (caso 3); lo que no
        // se toca es de quién es.
        assert_eq!(duenio(&destino), esperado, "el dueño y el grupo, intactos");
        assert_eq!(
            duenio(destino.parent().unwrap()),
            esperado,
            "y la `_revisar` que hemos creado para él, también"
        );
        let _ = fs::remove_dir_all(&raiz);
    }

    /// `FILE_MODE` es un modo final, no un mínimo: cuando está puesto, ni el
    /// archivo ni la carpeta opinan.
    #[test]
    fn el_modo_forzado_no_se_mezcla_con_el_del_archivo() {
        let forzado = Herencia {
            modo_archivo_forzado: Some(0o640),
            ..biblioteca()
        };
        assert_eq!(
            forzado.para_archivo(archivo(0o666, 7, 7)),
            (Some(0o640), Some(7), Some(7))
        );
        // Y `PUID`/`PGID` ganan al archivo, que es su razón de ser.
        let fijados = Herencia {
            uid_forzado: Some(1026),
            gid_forzado: Some(100),
            ..biblioteca()
        };
        assert_eq!(
            fijados.para_archivo(archivo(0o664, 7, 7)),
            (Some(0o664), Some(1026), Some(100))
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
            Referencia::Valida(0o777, 1026, 100)
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
