//! Orquestación headless: dado un archivo de vídeo, busca su título en TMDb,
//! decide nombre y carpeta de destino y lo mueve. Es la versión "sin interfaz"
//! de la lógica que en la GUI requería intervención del usuario.

use crate::cache::SeriesCache;
use crate::config::Idioma;
use crate::mover::mover_seguro;
use crate::parse::{clave_cache, extraer_info_archivo, limpiar_nombre_archivo, EpisodioInfo};
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

    let (titulo, ep_info) = extraer_info_archivo(&nombre);
    let como_serie = match opts.forzar_series {
        Some(v) => v,
        None => ep_info.is_some(),
    };

    if como_serie {
        let ep = match ep_info {
            Some(e) => e,
            None => return Resultado::NoEsSerieConEpisodio,
        };

        // Capa 4: si la serie ya está cacheada, saltamos la búsqueda.
        let clave = clave_cache(&titulo);
        if let Some(series_id) = cache.get(&clave) {
            match construir_y_mover_serie(client, opts, cache, series_id, ep, path, &ext) {
                Ok(destino) => {
                    return Resultado::Renombrado {
                        destino,
                        score: 1.0,
                        desde_cache: true,
                    }
                }
                // El id cacheado ya no existe en TMDb: invalidamos la entrada
                // y seguimos con una búsqueda normal (caché autocurativa).
                Err(Fallo::SerieNoExiste) => cache.eliminar(&clave),
                Err(Fallo::Red(e)) => return Resultado::ErrorRed(e),
                Err(Fallo::Definitivo(e)) => return Resultado::Error(e),
            }
        }

        let candidatos =
            match buscar_candidatos_serie(client, &titulo, &opts.api_key, opts.idioma) {
                Ok(c) => c,
                Err(e) => return Resultado::ErrorRed(e.to_string()),
            };
        if candidatos.is_empty() {
            return manejar_dudoso(opts, path, "sin resultados en TMDb");
        }

        let mejor = &candidatos[0];
        let confiado = mejor.score >= opts.umbral;
        if confiado || opts.accion_dudoso == AccionDudoso::Forzar {
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
        } else {
            manejar_dudoso(opts, path, &format!("match dudoso (score {:.2})", mejor.score))
        }
    } else {
        let candidatos =
            match buscar_candidatos_pelicula(client, &titulo, &opts.api_key, opts.idioma) {
                Ok(c) => c,
                Err(e) => return Resultado::ErrorRed(e.to_string()),
            };
        if candidatos.is_empty() {
            return manejar_dudoso(opts, path, "sin resultados en TMDb");
        }

        let mejor = &candidatos[0];
        let confiado = mejor.score >= opts.umbral;
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

fn construir_y_mover_serie(
    client: &Client,
    opts: &Opciones,
    cache: &mut SeriesCache,
    series_id: i64,
    ep: EpisodioInfo,
    path: &Path,
    ext: &str,
) -> Result<String, Fallo> {
    // Info de la serie: de la caché en memoria si ya se consultó en esta
    // ejecución (evita repetir la llamada con cada episodio del lote).
    let (titulo_serie, anio) = match cache.serie_info(series_id) {
        Some(info) => info.clone(),
        None => {
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
            info
        }
    };

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
