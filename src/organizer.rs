//! Orquestación headless: dado un archivo de vídeo, busca su título en TMDb,
//! decide nombre y carpeta de destino y lo mueve. Es la versión "sin interfaz"
//! de la lógica que en la GUI requería intervención del usuario.

use crate::cache::SeriesCache;
use crate::config::Idioma;
use crate::mover::mover_seguro;
use crate::parse::{
    anio_del_sufijo, clave_cache_titulo, cobertura_palabras, extraer_info_archivo,
    limpiar_nombre_archivo, palabras_fuertes, titulo_compuesto, titulo_episodio, EpisodioInfo,
};
use crate::tmdb::{
    buscar_candidatos_pelicula, buscar_candidatos_serie, buscar_coleccion_pelicula,
    buscar_nombre_serie, buscar_numeros_episodios, buscar_temporada, Candidato, ErrorTmdb,
};
use reqwest::blocking::Client;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Qué hacer cuando TMDb no da un resultado con confianza suficiente.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccionDudoso {
    /// Mover a la carpeta de revisión (cuarentena).
    Revisar,
    /// Dejar el archivo donde está y solo registrar un aviso.
    Dejar,
    /// Usar igualmente el mejor candidato aunque el score sea bajo.
    Forzar,
}

/// Configuración de procesamiento (derivada de las variables de entorno).
#[derive(Clone)]
pub struct Opciones {
    pub api_key: String,
    pub idioma: Idioma,
    pub dir_peliculas: PathBuf,
    pub dir_series: PathBuf,
    pub dir_revisar: PathBuf,
    /// `true` => formato `1x05`; `false` => formato `S01E05`.
    pub formato_1x05: bool,
    /// Umbral de score por encima del cual se renombra sin dudar.
    pub umbral: f64,
    /// `true` => estructura anidada Plex/Jellyfin; `false` => plana.
    pub anidado: bool,
    /// `None` => autodetectar (serie si el nombre tiene SxxExx/NxNN);
    /// `Some(true)` => forzar serie; `Some(false)` => forzar película.
    pub forzar_series: Option<bool>,
    pub accion_dudoso: AccionDudoso,
    /// Si una película pertenece a una colección/saga de TMDb, agruparla en una
    /// carpeta padre con el nombre de la colección **tal cual** lo da TMDb.
    /// Solo aplica en modo anidado.
    pub usar_colecciones: bool,
    /// Formato del año en carpetas/archivos: `true` => `Título [2021]`;
    /// `false` => `Título (2021)` (estándar Plex/Jellyfin).
    pub anio_corchetes: bool,
    /// Modo simulación: se calcula y registra el destino de cada archivo pero
    /// NO se mueve ni se borra nada. Útil para validar la configuración.
    pub dry_run: bool,
}

/// Margen de similitud dentro del cual dos candidatos se consideran empatados.
const EMPATE: f64 = 0.02;

/// ¿La elección del mejor candidato la está decidiendo el desempate y no la
/// similitud?
///
/// Un score alto no basta por sí solo. `Star Trek 1x01 …` puntúa **1.00**
/// contra la serie de 1966 aunque el archivo sea de 2022: ningún umbral lo
/// atrapa. Cuando varios candidatos quedan a menos de `EMPATE` del mejor, quien
/// decide de verdad es la popularidad, y eso no basta para renombrar sin
/// supervisión. El año ya está incorporado al score (bonus/penalización en
/// `tmdb`), así que un empate que llega hasta aquí es genuinamente indecidible.
fn hay_empate(candidatos: &[Candidato]) -> bool {
    empatados(candidatos).len() > 1
}

/// Índices de los candidatos que quedan a menos de `EMPATE` del mejor (el
/// propio mejor incluido). La lista viene ordenada por score descendente, así
/// que el primero siempre está dentro.
fn empatados(candidatos: &[Candidato]) -> Vec<usize> {
    let tope = candidatos[0].score;
    candidatos
        .iter()
        .enumerate()
        .filter(|(_, c)| tope - c.score <= EMPATE)
        .map(|(i, _)| i)
        .collect()
}

/// Rompe un empate preguntando a TMDb cuál de las series empatadas tiene de
/// verdad el episodio que trae el archivo.
///
/// Un empate por similitud no siempre es indecidible: `Silo 3x08` empata 1.00
/// entre *Silo* (2023) y *Silo* (2017), pero la segunda tiene una única
/// temporada de 4 capítulos. El nombre del archivo trae un dato que la
/// búsqueda por título no usa —temporada y episodio— y ese dato lo decide sin
/// heurísticas: manda TMDb, no la popularidad.
///
/// `tiene_episodio` se inyecta para poder probar la decisión sin red, y para
/// que la misma criba sirva a las dos preguntas que sabemos hacerle al
/// episodio: si existe (plan D) y si se titula como dice el archivo (plan E).
/// Solo se devuelve un ganador si sobrevive **exactamente uno**; dos
/// supervivientes siguen siendo un empate genuino y el archivo se va a
/// revisión como antes.
///
/// Un candidato no fiable (variante de búsqueda "desesperada") nunca gana,
/// pero sí puede impedir que gane otro: si también tiene el episodio, la duda
/// es real.
fn desempatar_por_episodio<F>(
    candidatos: &[Candidato],
    umbral: f64,
    mut tiene_episodio: F,
) -> Result<Option<Candidato>, String>
where
    F: FnMut(i64) -> Result<bool, String>,
{
    let empatados = empatados(candidatos);
    if empatados.len() < 2 {
        return Ok(None);
    }
    // Desempatar algo que ni resuelto llegaría al umbral no sirve de nada y
    // gastaría llamadas a TMDb: el archivo acabaría en revisión igualmente.
    if candidatos[0].score < umbral {
        return Ok(None);
    }

    let mut superviviente: Option<&Candidato> = None;
    for i in empatados {
        if !tiene_episodio(candidatos[i].id)? {
            continue;
        }
        if superviviente.is_some() {
            // Dos series empatadas tienen ese episodio: sigue sin decidirse.
            return Ok(None);
        }
        superviviente = Some(&candidatos[i]);
    }

    match superviviente {
        Some(c) if c.fiable => Ok(Some(c.clone())),
        _ => Ok(None),
    }
}

/// ¿Existe en TMDb el episodio `ep` dentro de esta serie? Un 404 de la
/// temporada es una respuesta válida (no la tiene), no un fallo.
fn serie_tiene_episodio(
    client: &Client,
    opts: &Opciones,
    series_id: i64,
    ep: EpisodioInfo,
) -> Result<bool, String> {
    match buscar_numeros_episodios(client, &opts.api_key, opts.idioma, series_id, ep.temporada) {
        Ok(numeros) => Ok(numeros.contains(&ep.episodio)),
        Err(ErrorTmdb::NoEncontrado) => Ok(false),
        Err(e @ ErrorTmdb::Red(_)) => Err(format!(
            "no se pudo comprobar el episodio {}x{:02} de la serie {}: {}",
            ep.temporada, ep.episodio, series_id, e
        )),
    }
}

/// Nombre que TMDb le da al episodio `ep` de una serie, apoyándose en la caché
/// de temporada en memoria: la primera consulta trae los nombres de la
/// temporada entera y las de los demás capítulos del lote salen gratis.
///
/// `Ok(None)` cuando no hay nombre que dar —la temporada no existe (un 404 es
/// una respuesta válida, no un fallo) o TMDb no ha titulado ese capítulo—, así
/// que un `None` **no** significa que el episodio no exista.
fn nombre_de_episodio(
    client: &Client,
    opts: &Opciones,
    cache: &mut SeriesCache,
    series_id: i64,
    ep: EpisodioInfo,
) -> Result<Option<String>, String> {
    if let Some(m) = cache.temporada(series_id, ep.temporada) {
        return Ok(m.get(&ep.episodio).cloned());
    }
    match buscar_temporada(
        client,
        &opts.api_key,
        opts.idioma,
        series_id,
        ep.temporada,
    ) {
        Ok(m) => {
            let nombre = m.get(&ep.episodio).cloned();
            cache.insertar_temporada(series_id, ep.temporada, m);
            Ok(nombre)
        }
        // Temporada inexistente en TMDb: seguimos sin nombre de episodio y
        // cacheamos el vacío para no volver a pedirla.
        Err(ErrorTmdb::NoEncontrado) => {
            cache.insertar_temporada(series_id, ep.temporada, HashMap::new());
            Ok(None)
        }
        Err(e @ ErrorTmdb::Red(_)) => Err(format!(
            "no se pudo obtener la temporada {}: {}",
            ep.temporada, e
        )),
    }
}

/// Cobertura mínima de palabras para dar por bueno que el título de capítulo
/// que trae el archivo y el que da TMDb son el mismo.
///
/// Se mide con `cobertura_palabras` y no con similitud por el mismo motivo que
/// en [`cache_es_de_fiar`]: el archivo trae el título recortado o sin tildes y
/// TMDb lo trae entero, así que lo que importa es cuántas de las palabras del
/// archivo aparecen en el nombre real, no cuánto se parecen letra a letra.
const COBERTURA_MINIMA_EPISODIO: f64 = 0.75;

/// ¿El episodio `ep` de esta serie se llama de verdad como dice el archivo?
///
/// Un `false` cubre dos casos que aquí dan igual: que la serie no tenga ese
/// episodio y que lo tenga con otro nombre. En ambos, esta serie no explica el
/// nombre del archivo.
fn episodio_se_llama(
    client: &Client,
    opts: &Opciones,
    cache: &mut SeriesCache,
    series_id: i64,
    ep: EpisodioInfo,
    titulo_archivo: &str,
) -> Result<bool, String> {
    match nombre_de_episodio(client, opts, cache, series_id, ep)? {
        Some(n) => Ok(cobertura_palabras(titulo_archivo, &n) >= COBERTURA_MINIMA_EPISODIO),
        None => Ok(false),
    }
}

/// ¿El mejor candidato basta para renombrar sin supervisión? Score por encima
/// del umbral, de una variante fiable y sin empate.
fn eleccion_firme(candidatos: &[Candidato], opts: &Opciones) -> bool {
    match candidatos.first() {
        Some(mejor) => mejor.score >= opts.umbral && mejor.fiable && !hay_empate(candidatos),
        None => false,
    }
}

/// Descripción legible de un empate, para el log y la carpeta de revisión.
fn motivo_empate(candidatos: &[Candidato]) -> String {
    let tope = candidatos[0].score;
    let lista: Vec<String> = candidatos
        .iter()
        .filter(|c| tope - c.score <= EMPATE)
        .take(3)
        .map(|c| format!("{} ({})", c.titulo, c.anio))
        .collect();
    format!("match ambiguo (score {:.2}) entre: {}", tope, lista.join(" · "))
}

/// Cobertura mínima de palabras para dar por buena una entrada de caché.
/// Los datos reales dejan un margen amplio: las entradas podridas medidas
/// quedaron en 0.33-0.40 y las correctas en 1.00.
const COBERTURA_MINIMA_CACHE: f64 = 0.6;

/// Margen de años tolerado entre el nombre del archivo y la serie cacheada.
/// TMDb fecha por primera emisión, que puede diferir un año del que trae el
/// archivo según el país; más de eso ya es otra serie.
const MARGEN_ANIOS_CACHE: u32 = 1;

/// ¿La serie a la que apunta un id cacheado se corresponde de verdad con lo
/// que dice el nombre del archivo?
///
/// Una entrada de caché errónea es permanente y **anula cualquier mejora
/// posterior del parser**: se consulta antes de buscar, así que el archivo se
/// renombra mal para siempre sin volver a preguntar a TMDb. Pasó de verdad:
/// `cache.json` traía `"star trek strange new worlds 2022" -> 253` (Star Trek,
/// 1966), escrito por una versión antigua, y sobrevivió a los arreglos del
/// parser porque nadie revalidaba el acierto.
///
/// Se comprueban dos señales independientes; con que falle una, se desconfía:
/// - **Año**: decisivo cuando se conoce por ambos lados (2022 contra 1966).
/// - **Cobertura de palabras**: caza los casos sin año, como
///   `"special ops lioness"` apuntando a *Special A* (2008).
fn cache_es_de_fiar(
    titulo_archivo: &str,
    anio_archivo: Option<u32>,
    nombre_serie: &str,
    anio_serie: &str,
) -> bool {
    if let (Some(a), Ok(b)) = (anio_archivo, anio_serie.parse::<u32>()) {
        if b != 0 && a.abs_diff(b) > MARGEN_ANIOS_CACHE {
            return false;
        }
    }
    cobertura_palabras(titulo_archivo, nombre_serie) >= COBERTURA_MINIMA_CACHE
}

/// Formatea `Título` + año según la preferencia: `Título (2021)` o `Título [2021]`.
fn titulo_con_anio(titulo: &str, anio: &str, corchetes: bool) -> String {
    if corchetes {
        format!("{} [{}]", titulo, anio)
    } else {
        format!("{} ({})", titulo, anio)
    }
}

/// Desenlace del procesamiento de un archivo (para el log del servicio).
pub enum Resultado {
    Renombrado {
        destino: String,
        score: f64,
        desde_cache: bool,
    },
    EnviadoARevisar {
        destino: String,
        motivo: String,
    },
    DejadoEnSitio {
        motivo: String,
    },
    NoEsSerieConEpisodio,
    /// Fallo transitorio de red/TMDb: el archivo NO se toca y conviene
    /// reintentarlo más tarde (a diferencia de `Error`, que no se reintenta).
    ErrorRed(String),
    Error(String),
}

/// Fallo interno al construir/mover, con la granularidad que necesita
/// `procesar_archivo` para decidir (reintentar, invalidar caché o rendirse).
enum Fallo {
    /// Red/TMDb caído: transitorio, reintentable.
    Red(String),
    /// El id de serie ya no existe en TMDb (caché obsoleta).
    SerieNoExiste,
    /// Error definitivo (p. ej. el destino ya existe): no se reintenta.
    Definitivo(String),
}

/// Procesa un único archivo de vídeo de principio a fin.
pub fn procesar_archivo(
    client: &Client,
    opts: &Opciones,
    cache: &mut SeriesCache,
    path: &Path,
) -> Resultado {
    let nombre = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let ext = match path.extension().and_then(|e| e.to_str()) {
        Some(e) => e.to_lowercase(),
        None => return Resultado::Error("el archivo no tiene extensión".to_string()),
    };

    let (titulo, anio_archivo, ep_info) = extraer_info_archivo(&nombre);
    let como_serie = match opts.forzar_series {
        Some(v) => v,
        None => ep_info.is_some(),
    };

    if como_serie {
        let ep = match ep_info {
            Some(e) => e,
            None => return Resultado::NoEsSerieConEpisodio,
        };

        // Capa 4: si la serie ya está cacheada, saltamos la búsqueda. Pero
        // antes comprobamos que el id cacheado siga cuadrando con el nombre del
        // archivo: una entrada mala escrita por una versión antigua no se
        // corrige sola, y al consultarse antes de buscar deja inservible
        // cualquier mejora del parser. Ver `cache_es_de_fiar`.
        let clave = clave_cache_titulo(&titulo, anio_archivo);
        if let Some(series_id) = cache.get(&clave) {
            match obtener_info_serie(client, opts, cache, series_id) {
                Ok((nombre_serie, anio_serie)) => {
                    if cache_es_de_fiar(&titulo, anio_archivo, &nombre_serie, &anio_serie) {
                        match construir_y_mover_serie(
                            client, opts, cache, series_id, ep, path, &ext,
                        ) {
                            Ok(destino) => {
                                return Resultado::Renombrado {
                                    destino,
                                    score: 1.0,
                                    desde_cache: true,
                                }
                            }
                            Err(Fallo::SerieNoExiste) => cache.eliminar(&clave),
                            Err(Fallo::Red(e)) => return Resultado::ErrorRed(e),
                            Err(Fallo::Definitivo(e)) => return Resultado::Error(e),
                        }
                    } else {
                        // Apunta a otra serie: se tira la entrada y se busca de
                        // nuevo. La búsqueda, si acierta con confianza, la
                        // sobrescribe con el id bueno (caché autocurativa).
                        cache.eliminar(&clave);
                    }
                }
                // El id cacheado ya no existe en TMDb: misma cura.
                Err(Fallo::SerieNoExiste) => cache.eliminar(&clave),
                Err(Fallo::Red(e)) => return Resultado::ErrorRed(e),
                Err(Fallo::Definitivo(e)) => return Resultado::Error(e),
            }
        }

        let candidatos = match buscar_candidatos_serie(
            client,
            &titulo,
            anio_archivo,
            &opts.api_key,
            opts.idioma,
        ) {
            Ok(c) => c,
            Err(e) => return Resultado::ErrorRed(e.to_string()),
        };

        // Plan B: el código de episodio puede partir el nombre de la serie por
        // la mitad ("Star Trek 2x10 Strange New Worlds (2022)"), y entonces el
        // prefijo solo es un título genérico que empata con media docena de
        // series. Se reintenta con las dos mitades unidas. Solo entra cuando la
        // primera búsqueda no ha convencido, así que no puede estropear ningún
        // nombre que ya funcione: lo que hoy acaba en `_revisar` es lo único
        // que cambia de desenlace, y quien decide sigue siendo TMDb.
        let (candidatos, clave) = match eleccion_firme(&candidatos, opts) {
            true => (candidatos, clave),
            false => match titulo_compuesto(&nombre) {
                Some((titulo2, anio2)) => {
                    let clave2 = clave_cache_titulo(&titulo2, anio2);
                    match buscar_candidatos_serie(
                        client,
                        &titulo2,
                        anio2,
                        &opts.api_key,
                        opts.idioma,
                    ) {
                        Ok(c2) if eleccion_firme(&c2, opts) => (c2, clave2),
                        Ok(_) => (candidatos, clave),
                        Err(e) => return Resultado::ErrorRed(e.to_string()),
                    }
                }
                None => (candidatos, clave),
            },
        };

        // Plan C: exprimir el año que vive DETRÁS del código de episodio, que
        // `extraer_info_archivo` descarta por si es el año de emisión del
        // capítulo. En `Silo 3x07 Radio (2023)` es el año de la serie, y es lo
        // único que separa a las dos series llamadas "Silo" (2023 y 2017), que
        // empatan a 1.00 de similitud. Con el año, `tmdb` premia a una y
        // penaliza a la otra, y el empate desaparece.
        //
        // La clave de caché NO se toca: se memoriza bajo el título sin año
        // ("silo"), que es la clave que generan los demás episodios del lote
        // aunque no traigan año en el nombre. Así el reintento se paga una vez.
        let candidatos = match eleccion_firme(&candidatos, opts) {
            true => candidatos,
            false => match anio_del_sufijo(&nombre) {
                Some(anio) => match buscar_candidatos_serie(
                    client,
                    &titulo,
                    Some(anio),
                    &opts.api_key,
                    opts.idioma,
                ) {
                    Ok(c2) if eleccion_firme(&c2, opts) => c2,
                    Ok(_) => candidatos,
                    Err(e) => return Resultado::ErrorRed(e.to_string()),
                },
                None => candidatos,
            },
        };

        if candidatos.is_empty() {
            return manejar_dudoso(opts, path, "sin resultados en TMDb");
        }

        // Plan D: si sigue habiendo empate, que lo rompa el propio episodio.
        // Es el último dato del nombre que nadie ha usado todavía y el más
        // difícil de falsear: una serie homónima que no tiene esa temporada
        // queda descartada sin depender de la popularidad.
        let candidatos = match eleccion_firme(&candidatos, opts) {
            true => candidatos,
            false => match desempatar_por_episodio(&candidatos, opts.umbral, |id| {
                serie_tiene_episodio(client, opts, id, ep)
            }) {
                Ok(Some(ganador)) => vec![ganador],
                Ok(None) => candidatos,
                Err(e) => return Resultado::ErrorRed(e),
            },
        };

        // Plan E: cuando las dos homónimas tienen ese episodio, el plan D se
        // queda sin argumentos y hay que mirar CÓMO se llama el capítulo.
        // `Lucky 1x06` empata a 1.00 entre `Lucky` (2026) y `Lucky` (2007), y
        // las dos tienen un 1x06, pero solo la de 2026 lo titula "Vayas donde
        // vayas, siempre serás tú"; la de 2007 lo deja en "Episodio 6".
        //
        // Es el último dato del nombre que quedaba sin usar. Si el sufijo no
        // era el título del capítulo sino ruido de release, no encaja con
        // ningún candidato y el desempate se descarta sin efecto, igual que
        // los planes B y C: esto solo puede rescatar lo que hoy va a
        // `_revisar`, nunca estropear un nombre que ya funciona.
        let candidatos = match eleccion_firme(&candidatos, opts) {
            true => candidatos,
            // Un sufijo sin palabras fuertes ("de la") daría cobertura 1.00
            // contra cualquier cosa: no distingue nada y solo gasta llamadas.
            false => match titulo_episodio(&nombre).filter(|t| !palabras_fuertes(t).is_empty()) {
                Some(titulo_ep) => {
                    match desempatar_por_episodio(&candidatos, opts.umbral, |id| {
                        episodio_se_llama(client, opts, cache, id, ep, &titulo_ep)
                    }) {
                        Ok(Some(ganador)) => vec![ganador],
                        Ok(None) => candidatos,
                        Err(e) => return Resultado::ErrorRed(e),
                    }
                }
                None => candidatos,
            },
        };

        let mejor = &candidatos[0];
        // Un candidato de variante desesperada nunca auto-renombra por score.
        // Un empate tampoco: ver `hay_empate`.
        let empate = hay_empate(&candidatos);
        let confiado = eleccion_firme(&candidatos, opts);
        if confiado || opts.accion_dudoso == AccionDudoso::Forzar {
            // Solo se memoriza una identificación firme: cachear un empate
            // propagaría el error a todos los episodios del lote y lo dejaría
            // grabado en cache.json.
            if confiado {
                cache.insertar(clave, mejor.id);
            }
            let score = mejor.score;
            let id = mejor.id;
            match construir_y_mover_serie(client, opts, cache, id, ep, path, &ext) {
                Ok(destino) => Resultado::Renombrado {
                    destino,
                    score,
                    desde_cache: false,
                },
                Err(Fallo::Red(e)) => Resultado::ErrorRed(e),
                Err(Fallo::SerieNoExiste) => {
                    Resultado::Error(format!("la serie {} no existe en TMDb", id))
                }
                Err(Fallo::Definitivo(e)) => Resultado::Error(e),
            }
        } else if empate {
            manejar_dudoso(opts, path, &motivo_empate(&candidatos))
        } else {
            manejar_dudoso(opts, path, &format!("match dudoso (score {:.2})", mejor.score))
        }
    } else {
        let candidatos = match buscar_candidatos_pelicula(
            client,
            &titulo,
            anio_archivo,
            &opts.api_key,
            opts.idioma,
        ) {
            Ok(c) => c,
            Err(e) => return Resultado::ErrorRed(e.to_string()),
        };
        if candidatos.is_empty() {
            return manejar_dudoso(opts, path, "sin resultados en TMDb");
        }

        let mejor = &candidatos[0];
        // Un candidato de variante desesperada nunca auto-renombra por score.
        // Un empate tampoco: ver `hay_empate`.
        let empate = hay_empate(&candidatos);
        let confiado = mejor.score >= opts.umbral && mejor.fiable && !empate;
        if confiado || opts.accion_dudoso == AccionDudoso::Forzar {
            let score = mejor.score;
            match construir_y_mover_pelicula(client, opts, mejor, path, &ext) {
                Ok(destino) => Resultado::Renombrado {
                    destino,
                    score,
                    desde_cache: false,
                },
                Err(Fallo::Red(e)) => Resultado::ErrorRed(e),
                Err(Fallo::SerieNoExiste) => {
                    Resultado::Error(format!("la película {} no existe en TMDb", mejor.id))
                }
                Err(Fallo::Definitivo(e)) => Resultado::Error(e),
            }
        } else if empate {
            manejar_dudoso(opts, path, &motivo_empate(&candidatos))
        } else {
            manejar_dudoso(opts, path, &format!("match dudoso (score {:.2})", mejor.score))
        }
    }
}

/// Aplica la política de "match dudoso". Para `Forzar` sin candidato útil se
/// degrada a `Revisar` (no hay nada que forzar).
fn manejar_dudoso(opts: &Opciones, path: &Path, motivo: &str) -> Resultado {
    let accion = if opts.accion_dudoso == AccionDudoso::Forzar {
        AccionDudoso::Revisar
    } else {
        opts.accion_dudoso
    };

    match accion {
        AccionDudoso::Dejar => Resultado::DejadoEnSitio {
            motivo: motivo.to_string(),
        },
        AccionDudoso::Revisar | AccionDudoso::Forzar => {
            let nombre = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let destino = opts.dir_revisar.join(&nombre);
            if opts.dry_run {
                return Resultado::EnviadoARevisar {
                    destino: destino.display().to_string(),
                    motivo: motivo.to_string(),
                };
            }
            match mover_seguro(path, &destino) {
                Ok(_) => Resultado::EnviadoARevisar {
                    destino: destino.display().to_string(),
                    motivo: motivo.to_string(),
                },
                Err(e) => Resultado::Error(e),
            }
        }
    }
}

fn codigo_episodio(opts: &Opciones, ep: EpisodioInfo) -> String {
    if opts.formato_1x05 {
        format!("{}x{:02}", ep.temporada, ep.episodio)
    } else {
        format!("S{:02}E{:02}", ep.temporada, ep.episodio)
    }
}

/// Nombre y año de una serie por id. Se memoriza en la caché en memoria para
/// no repetir la llamada con cada episodio del mismo lote, así que consultarla
/// antes de mover (para validar el acierto) no cuesta ninguna petición extra.
fn obtener_info_serie(
    client: &Client,
    opts: &Opciones,
    cache: &mut SeriesCache,
    series_id: i64,
) -> Result<(String, String), Fallo> {
    if let Some(info) = cache.serie_info(series_id) {
        return Ok(info.clone());
    }
    let info = match buscar_nombre_serie(client, &opts.api_key, opts.idioma, series_id) {
        Ok(v) => v,
        Err(ErrorTmdb::NoEncontrado) => return Err(Fallo::SerieNoExiste),
        Err(e @ ErrorTmdb::Red(_)) => {
            return Err(Fallo::Red(format!(
                "no se pudo obtener info de la serie {}: {}",
                series_id, e
            )))
        }
    };
    cache.insertar_serie_info(series_id, info.0.clone(), info.1.clone());
    Ok(info)
}

fn construir_y_mover_serie(
    client: &Client,
    opts: &Opciones,
    cache: &mut SeriesCache,
    series_id: i64,
    ep: EpisodioInfo,
    path: &Path,
    ext: &str,
) -> Result<String, Fallo> {
    let (titulo_serie, anio) = obtener_info_serie(client, opts, cache, series_id)?;
    let nombre_ep = nombre_de_episodio(client, opts, cache, series_id, ep).map_err(Fallo::Red)?;

    let titulo_limpio = limpiar_nombre_archivo(&titulo_serie);
    let codigo = codigo_episodio(opts, ep);
    let base = match nombre_ep {
        Some(n) => format!("{} {} {}", titulo_limpio, codigo, limpiar_nombre_archivo(&n)),
        None => format!("{} {}", titulo_limpio, codigo),
    };
    // Un título de episodio que acaba en punto ("Hagas lo que hagas, no
    // vuelvas a casa.") pegaba el punto de la extensión y salía un
    // "...casa..mkv". Además, un nombre terminado en punto o espacio es
    // inválido en Windows/SMB, que es por donde se ve la biblioteca.
    let archivo = format!("{}.{}", base.trim_end_matches(['.', ' ']), ext);

    let destino = if opts.anidado {
        let carpeta_serie = if anio != "0000" {
            titulo_con_anio(&titulo_limpio, &anio, opts.anio_corchetes)
        } else {
            titulo_limpio.clone()
        };
        opts.dir_series
            .join(carpeta_serie)
            .join(format!("Season {:02}", ep.temporada))
            .join(&archivo)
    } else {
        opts.dir_series.join(&archivo)
    };

    if opts.dry_run {
        return Ok(destino.display().to_string());
    }
    mover_seguro(path, &destino).map_err(Fallo::Definitivo)?;
    Ok(destino.display().to_string())
}

fn construir_y_mover_pelicula(
    client: &Client,
    opts: &Opciones,
    candidato: &Candidato,
    path: &Path,
    ext: &str,
) -> Result<String, Fallo> {
    let titulo_limpio = limpiar_nombre_archivo(&candidato.titulo);
    let tiene_anio = candidato.anio != "0000";

    let destino = if opts.anidado {
        let carpeta_pelicula = if tiene_anio {
            titulo_con_anio(&titulo_limpio, &candidato.anio, opts.anio_corchetes)
        } else {
            titulo_limpio.clone()
        };
        let archivo = format!("{}.{}", carpeta_pelicula, ext);

        // Si pertenece a una colección/saga, anteponemos una carpeta con el
        // nombre de la colección tal cual lo da TMDb (solo saneado de
        // caracteres inválidos para el sistema de archivos).
        let mut base_dir = opts.dir_peliculas.clone();
        if opts.usar_colecciones {
            match buscar_coleccion_pelicula(client, &opts.api_key, opts.idioma, candidato.id) {
                Ok(Some(coleccion)) => {
                    let coleccion_limpia = limpiar_nombre_archivo(&coleccion);
                    if !coleccion_limpia.is_empty() {
                        base_dir = base_dir.join(coleccion_limpia);
                    }
                }
                Ok(None) => {}
                // Con la red caída no sabemos si tiene colección: mejor
                // reintentar luego que colocarla sin su carpeta de saga.
                Err(e @ ErrorTmdb::Red(_)) => {
                    return Err(Fallo::Red(format!(
                        "no se pudo comprobar la colección: {}",
                        e
                    )))
                }
                Err(ErrorTmdb::NoEncontrado) => {}
            }
        }
        base_dir.join(&carpeta_pelicula).join(&archivo)
    } else {
        let base = if tiene_anio {
            titulo_con_anio(&titulo_limpio, &candidato.anio, opts.anio_corchetes)
        } else {
            titulo_limpio.clone()
        };
        opts.dir_peliculas.join(format!("{}.{}", base, ext))
    };

    if opts.dry_run {
        return Ok(destino.display().to_string());
    }
    mover_seguro(path, &destino).map_err(Fallo::Definitivo)?;
    Ok(destino.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(titulo: &str, anio: &str, score: f64, pop: f64) -> Candidato {
        cand_id(1, titulo, anio, score, pop)
    }

    fn cand_id(id: i64, titulo: &str, anio: &str, score: f64, pop: f64) -> Candidato {
        Candidato {
            id,
            media_type: "tv".to_string(),
            titulo: titulo.to_string(),
            nombre_original: titulo.to_string(),
            anio: anio.to_string(),
            popularidad: pop,
            overview: String::new(),
            score,
            fiable: true,
        }
    }

    #[test]
    fn un_ganador_claro_no_empata() {
        let cs = vec![
            cand("Severance", "2022", 1.0, 50.0),
            cand("Severance Otra", "2010", 0.80, 90.0),
        ];
        assert!(!hay_empate(&cs));
    }

    #[test]
    fn dos_candidatos_igualados_empatan() {
        // Dos "Dune" con similitud identica: sin anio que los separe, quien
        // decide es la popularidad, y eso no basta para renombrar solo.
        let cs = vec![
            cand("Dune", "2021", 1.0, 90.0),
            cand("Dune", "1984", 1.0, 30.0),
        ];
        assert!(hay_empate(&cs));
    }

    #[test]
    fn la_penalizacion_por_anio_deshace_el_empate() {
        // Con anio en el archivo, `tmdb` ya premia al que coincide y penaliza
        // al resto; la diferencia resultante supera el margen de empate.
        let cs = vec![
            cand("Dune", "1984", 1.05, 30.0),
            cand("Dune", "2021", 0.85, 90.0),
        ];
        assert!(!hay_empate(&cs));
    }

    #[test]
    fn diferencia_por_encima_del_margen_no_empata() {
        // El caso Star Trek ya resuelto por el parser: 0.9931 vs 0.8643.
        let cs = vec![
            cand("Star Trek: Strange New Worlds", "2022", 0.9931, 60.0),
            cand("Star Trek", "1966", 0.8643, 61.0),
        ];
        assert!(!hay_empate(&cs));
    }

    #[test]
    fn el_anio_delata_una_entrada_de_cache_de_otra_serie() {
        // Caso real: cache.json traía "star trek strange new worlds 2022" -> 253
        // (Star Trek, 1966), escrito por una version antigua del parser. El
        // archivo dice 2022 y la serie es del 66: no hay duda posible.
        assert!(!cache_es_de_fiar(
            "Star Trek Strange New Worlds",
            Some(2022),
            "Star Trek",
            "1966"
        ));
    }

    #[test]
    fn la_cobertura_delata_una_entrada_podrida_sin_anio() {
        // El otro caso real: "special ops lioness" apuntaba a Special A, un
        // anime de 2008. Sin anio en el nombre del archivo, quien lo caza es la
        // cobertura de palabras ("ops" y "lioness" no salen en "Special A").
        assert!(!cache_es_de_fiar("Special Ops Lioness", None, "Special A", "2008"));
    }

    /// Los dos "Silo" reales de TMDb: la de 2023 (10 capitulos por temporada,
    /// 3 temporadas) y una de 2017 con una sola temporada de 4.
    fn los_dos_silo() -> Vec<Candidato> {
        vec![
            cand_id(125988, "Silo", "2023", 1.0, 301.05),
            cand_id(256215, "Silo", "2017", 1.0, 1.15),
        ]
    }

    #[test]
    fn el_episodio_rompe_el_empate_entre_series_homonimas() {
        // Caso real de `Silo.3x08`: empate a 1.00 que mandaba el archivo a
        // revision. Solo una de las dos tiene una temporada 3.
        let cs = los_dos_silo();
        assert!(hay_empate(&cs));
        let ganador = desempatar_por_episodio(&cs, 0.85, |id| Ok(id == 125988))
            .unwrap()
            .expect("deberia quedar una sola serie con ese episodio");
        assert_eq!(ganador.id, 125988);
    }

    #[test]
    fn si_las_dos_tienen_el_episodio_el_empate_sigue_en_pie() {
        // Desempatar no es elegir: con dos series que de verdad podrian ser,
        // el archivo tiene que seguir yendo a revision.
        let cs = los_dos_silo();
        assert!(desempatar_por_episodio(&cs, 0.85, |_| Ok(true))
            .unwrap()
            .is_none());
    }

    #[test]
    fn si_ninguna_tiene_el_episodio_no_se_inventa_un_ganador() {
        let cs = los_dos_silo();
        assert!(desempatar_por_episodio(&cs, 0.85, |_| Ok(false))
            .unwrap()
            .is_none());
    }

    /// Las dos series llamadas exactamente "Lucky": la de 2026, con capitulos
    /// titulados, y la de 2007, con 41 capitulos en una sola temporada
    /// llamados "Episodio N".
    fn los_dos_lucky() -> Vec<Candidato> {
        vec![
            cand_id(278624, "Lucky", "2026", 1.0, 74.7),
            cand_id(58791, "Lucky", "2007", 1.0, 3.6),
        ]
    }

    /// Nombres reales que TMDb da al 1x06 de cada una de las dos "Lucky".
    fn nombre_1x06(id: i64) -> &'static str {
        match id {
            278624 => "Vayas donde vayas, siempre serás tú",
            _ => "Episodio 6",
        }
    }

    #[test]
    fn el_titulo_del_capitulo_rompe_el_empate_que_el_episodio_no_puede() {
        // Caso real de `Lucky.1x06`: empate a 1.00 y las DOS series tienen un
        // 1x06, asi que preguntar si el episodio existe no decide nada. Lo que
        // las separa es como se llama.
        let cs = los_dos_lucky();
        let suf = titulo_episodio(
            "Lucky.1x06.Vayas.donde.vayas,.siempre.serás.tú.WEBRip.1080p.x265-EAC3.mkv",
        )
        .expect("el sufijo es el titulo del capitulo");

        assert!(desempatar_por_episodio(&cs, 0.85, |_| Ok(true))
            .unwrap()
            .is_none());

        let ganador = desempatar_por_episodio(&cs, 0.85, |id| {
            Ok(cobertura_palabras(&suf, nombre_1x06(id)) >= COBERTURA_MINIMA_EPISODIO)
        })
        .unwrap()
        .expect("solo una de las dos titula asi su 1x06");
        assert_eq!(ganador.id, 278624);
    }

    #[test]
    fn la_cobertura_separa_el_titulo_de_capitulo_del_generico() {
        // Fija la medicion con la que se eligio COBERTURA_MINIMA_EPISODIO,
        // sobre los nombres reales de TMDb: no hay zona gris que afinar, el
        // correcto cubre entero y el generico de la homonima no cubre nada.
        // Aguanta las dos deformaciones habituales de un release: perder las
        // tildes y traer el titulo recortado.
        let real = nombre_1x06(278624);
        for suf in [
            "Vayas donde vayas, siempre serás tú",
            "Vayas donde vayas siempre seras tu",
            "Vayas donde vayas",
        ] {
            assert_eq!(cobertura_palabras(suf, real), 1.0);
            assert_eq!(cobertura_palabras(suf, nombre_1x06(58791)), 0.0);
        }
    }

    #[test]
    fn un_empate_por_debajo_del_umbral_no_gasta_llamadas() {
        // Aunque se resolviera, seguiria sin llegar al umbral: ni se pregunta.
        let cs = vec![
            cand_id(1, "Otra cosa", "2023", 0.60, 10.0),
            cand_id(2, "Otra cosa", "2017", 0.60, 1.0),
        ];
        let mut preguntas = 0;
        let r = desempatar_por_episodio(&cs, 0.85, |_| {
            preguntas += 1;
            Ok(true)
        });
        assert!(r.unwrap().is_none());
        assert_eq!(preguntas, 0);
    }

    #[test]
    fn sin_empate_no_se_desempata_nada() {
        let cs = vec![
            cand_id(1, "Severance", "2022", 1.0, 50.0),
            cand_id(2, "Severance Otra", "2010", 0.80, 90.0),
        ];
        assert!(desempatar_por_episodio(&cs, 0.85, |_| Ok(true))
            .unwrap()
            .is_none());
    }

    #[test]
    fn un_candidato_no_fiable_no_gana_el_desempate() {
        // Viene de una variante "desesperada" (una palabra suelta): puede
        // bloquear el desempate, pero nunca renombrar por su cuenta.
        let mut cs = los_dos_silo();
        cs[0].fiable = false;
        assert!(desempatar_por_episodio(&cs, 0.85, |id| Ok(id == 125988))
            .unwrap()
            .is_none());
    }

    #[test]
    fn un_fallo_de_red_al_desempatar_no_manda_el_archivo_a_revision() {
        // Sin respuesta de TMDb no se sabe nada: el llamador debe reintentar,
        // no dar por dudoso lo que quiza no lo es.
        let cs = los_dos_silo();
        assert!(desempatar_por_episodio(&cs, 0.85, |_| Err("sin red".to_string())).is_err());
    }

    #[test]
    fn la_similitud_no_habria_detectado_ninguno_de_los_dos() {
        // Por que la comprobacion NO usa Jaro-Winkler: las dos entradas
        // podridas puntuan por encima de una entrada correcta, asi que ningun
        // umbral de similitud las separa. Este test fija esa medicion para que
        // nadie "simplifique" la cobertura sustituyendola por similitud.
        use crate::parse::similitud;
        let podrido_a = similitud("star trek strange new worlds", "Star Trek", "Star Trek");
        let podrido_b = similitud("special ops lioness", "Special A", "Special A");
        let bueno = similitud(
            "marshals",
            "Marshals: Una historia de Yellowstone",
            "Marshals: Una historia de Yellowstone",
        );
        assert!(podrido_a > bueno, "{podrido_a} vs {bueno}");
        assert!(podrido_b > bueno, "{podrido_b} vs {bueno}");
    }

    #[test]
    fn una_entrada_correcta_sobrevive_a_la_validacion() {
        // Ninguna de estas debe tirarse: subtitulo que el archivo no trae,
        // tildes, y el desfase de un anio que TMDb a veces tiene por pais.
        assert!(cache_es_de_fiar(
            "Marshals",
            None,
            "Marshals: Una historia de Yellowstone",
            "2026"
        ));
        assert!(cache_es_de_fiar("Teheran", None, "Teherán", "2020"));
        assert!(cache_es_de_fiar("33 dias", None, "33 días", "2026"));
        assert!(cache_es_de_fiar(
            "Star Trek Strange New Worlds",
            Some(2022),
            "Star Trek: Strange New Worlds",
            "2022"
        ));
        assert!(cache_es_de_fiar("Silo", Some(2023), "Silo", "2024"));
    }

    #[test]
    fn sin_anio_en_el_archivo_no_se_descarta_por_anio() {
        // El archivo no aporta anio: la comprobacion de anio no debe opinar, y
        // manda la cobertura, que aqui es total.
        assert!(cache_es_de_fiar("Ted Lasso", None, "Ted Lasso", "2020"));
    }

    #[test]
    fn el_motivo_nombra_a_los_empatados() {
        let cs = vec![
            cand("Dune", "2021", 1.0, 90.0),
            cand("Dune", "1984", 1.0, 30.0),
        ];
        let m = motivo_empate(&cs);
        assert!(m.contains("Dune (2021)"), "{m}");
        assert!(m.contains("Dune (1984)"), "{m}");
    }
}
