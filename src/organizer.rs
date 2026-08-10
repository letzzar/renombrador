//! Orquestación headless: dado un archivo de vídeo, busca su título en TMDb,
//! decide nombre y carpeta de destino y lo mueve. Es la versión "sin interfaz"
//! de la lógica que en la GUI requería intervención del usuario.

use crate::cache::SeriesCache;
use crate::config::Idioma;
use crate::mover::mover_seguro;
use crate::parse::{
    clave_cache_titulo, cobertura_palabras, extraer_info_archivo, limpiar_nombre_archivo,
    EpisodioInfo,
};
use crate::tmdb::{
    buscar_candidatos_pelicula, buscar_candidatos_serie, buscar_coleccion_pelicula,
    buscar_nombre_serie, buscar_temporada, Candidato, ErrorTmdb,
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
    let tope = candidatos[0].score;
    candidatos
        .iter()
        .filter(|c| tope - c.score <= EMPATE)
        .count()
        > 1
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
        if candidatos.is_empty() {
            return manejar_dudoso(opts, path, "sin resultados en TMDb");
        }

        let mejor = &candidatos[0];
        // Un candidato de variante desesperada nunca auto-renombra por score.
        // Un empate tampoco: ver `hay_empate`.
        let empate = hay_empate(&candidatos);
        let confiado = mejor.score >= opts.umbral && mejor.fiable && !empate;
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

    // Nombre del episodio: de la caché de temporada si existe; si no, UNA
    // llamada trae los nombres de toda la temporada y se cachea.
    let nombre_ep = match cache.temporada(series_id, ep.temporada) {
        Some(m) => m.get(&ep.episodio).cloned(),
        None => match buscar_temporada(
            client,
            &opts.api_key,
            opts.idioma,
            series_id,
            ep.temporada,
        ) {
            Ok(m) => {
                let nombre = m.get(&ep.episodio).cloned();
                cache.insertar_temporada(series_id, ep.temporada, m);
                nombre
            }
            // Temporada inexistente en TMDb: seguimos sin nombre de episodio
            // y cacheamos el vacío para no volver a pedirla.
            Err(ErrorTmdb::NoEncontrado) => {
                cache.insertar_temporada(series_id, ep.temporada, HashMap::new());
                None
            }
            Err(e @ ErrorTmdb::Red(_)) => {
                return Err(Fallo::Red(format!(
                    "no se pudo obtener la temporada {}: {}",
                    ep.temporada, e
                )))
            }
        },
    };

    let titulo_limpio = limpiar_nombre_archivo(&titulo_serie);
    let codigo = codigo_episodio(opts, ep);
    let base = match nombre_ep {
        Some(n) => format!("{} {} {}", titulo_limpio, codigo, limpiar_nombre_archivo(&n)),
        None => format!("{} {}", titulo_limpio, codigo),
    };
    let archivo = format!("{}.{}", base, ext);

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
        Candidato {
            id: 1,
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
