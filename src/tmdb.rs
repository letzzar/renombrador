//! Llamadas a la API de TMDb. Lógica portada de la app original.

use crate::config::Idioma;
use crate::parse::{similitud, variantes_busqueda};
use reqwest::blocking::Client;
use serde_json::Value;
use std::collections::HashMap;

pub const BASE_URL: &str = "https://api.themoviedb.org/3";

/// Error al consultar TMDb. Distinguir ambos casos es importante: un fallo de
/// red NO significa "este contenido no existe", así que el llamador no debe
/// tratar el archivo como dudoso (enviarlo a cuarentena) sino reintentar.
#[derive(Debug)]
pub enum ErrorTmdb {
    /// Fallo de transporte o del servidor (sin red, timeout, HTTP no-2xx,
    /// respuesta ilegible…). Transitorio: tiene sentido reintentar más tarde.
    Red(String),
    /// TMDb respondió bien pero el recurso no existe (HTTP 404).
    NoEncontrado,
}

impl std::fmt::Display for ErrorTmdb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ErrorTmdb::Red(e) => write!(f, "TMDb no accesible: {}", e),
            ErrorTmdb::NoEncontrado => write!(f, "no existe en TMDb"),
        }
    }
}

/// GET a TMDb que separa 404 (no existe) del resto de fallos (red/servidor).
fn get_json(
    client: &Client,
    url: &str,
    params: &[(&str, &str)],
) -> Result<Value, ErrorTmdb> {
    let resp = client
        .get(url)
        .query(params)
        .send()
        .map_err(|e| ErrorTmdb::Red(e.to_string()))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(ErrorTmdb::NoEncontrado);
    }
    if !resp.status().is_success() {
        return Err(ErrorTmdb::Red(format!("HTTP {}", resp.status())));
    }
    resp.json().map_err(|e| ErrorTmdb::Red(e.to_string()))
}

/// Cuántos candidatos como máximo se conservan tras una búsqueda.
pub const MAX_CANDIDATOS: usize = 5;

/// Resultado bruto de TMDb (serie o película) ya puntuado.
#[derive(Clone, Debug)]
pub struct Candidato {
    pub id: i64,
    pub media_type: String, // "tv" o "movie"
    pub titulo: String,
    pub nombre_original: String,
    pub anio: String,
    pub popularidad: f64,
    pub overview: String,
    pub score: f64, // similitud calculada contra la query original
    /// `false` si el candidato solo apareció en una variante de búsqueda
    /// "desesperada" (palabra suelta): no debe auto-renombrar por muy alto
    /// que sea su score, solo ofrecerse como sugerencia.
    pub fiable: bool,
}

/// Bonus de score cuando el año del candidato coincide (±1) con el del archivo.
const BONUS_ANIO: f64 = 0.05;
/// Penalización cuando el archivo trae año y el candidato es de otro año.
/// Suficiente para tumbar homónimos por debajo del umbral de auto-renombrado
/// (p. ej. una peli de 1933 para un archivo marcado "(2025)").
const PENALIZACION_ANIO: f64 = 0.15;

/// Año ("0000" si falta) a partir de una fecha ISO `YYYY-MM-DD` de TMDb.
fn anio_de_fecha(fecha: &str) -> String {
    if fecha.len() >= 4 {
        fecha[..4].to_string()
    } else {
        "0000".to_string()
    }
}

/// Búsqueda multi-variante común a series y películas. Los endpoints
/// `search/tv` y `search/movie` solo difieren en el nombre de los campos.
///
/// Si el archivo trae año (`anio`), cada variante se busca primero filtrada
/// por año (`primary_release_year` / `first_air_date_year`); si ese filtro no
/// devuelve nada (año del release mal puesto), se reintenta sin él. El año
/// NUNCA va incrustado en la query: la API devuelve 0 resultados para
/// `"Dune 2021"`.
///
/// Devuelve `Ok(vec![])` solo cuando TMDb respondió bien y de verdad no hay
/// resultados; si no se obtuvo nada Y alguna petición falló, devuelve
/// `Err(Red)` para que el llamador reintente en vez de dar el archivo por
/// no identificable.
fn buscar_candidatos(
    client: &Client,
    titulo: &str,
    anio: Option<u32>,
    api_key: &str,
    idioma: Idioma,
    media_type: &str, // "tv" o "movie"
) -> Result<Vec<Candidato>, ErrorTmdb> {
    let (endpoint, campo_titulo, campo_original, campo_fecha, campo_anio) = if media_type == "tv" {
        ("search/tv", "name", "original_name", "first_air_date", "first_air_date_year")
    } else {
        ("search/movie", "title", "original_title", "release_date", "primary_release_year")
    };
    let url = format!("{}/{}", BASE_URL, endpoint);
    let variantes = variantes_busqueda(titulo, idioma);
    let anio_str = anio.map(|a| a.to_string());
    let mut todos: Vec<Candidato> = Vec::new();
    let mut ids_vistos: std::collections::HashSet<i64> = std::collections::HashSet::new();
    let mut error_red: Option<String> = None;

    for variante in &variantes {
        // Intentos por variante: con filtro de año (si lo hay) y sin él.
        let mut intentos: Vec<Option<&str>> = Vec::new();
        if let Some(a) = &anio_str {
            intentos.push(Some(a.as_str()));
        }
        intentos.push(None);

        for filtro_anio in intentos {
            let mut params = vec![
                ("api_key", api_key),
                ("query", variante.query.as_str()),
                ("language", variante.idioma.codigo()),
            ];
            if let Some(a) = filtro_anio {
                params.push((campo_anio, a));
            }
            let datos = match get_json(client, &url, &params) {
                Ok(d) => d,
                Err(ErrorTmdb::NoEncontrado) => continue, // raro en /search; sin resultados
                Err(ErrorTmdb::Red(e)) => {
                    error_red.get_or_insert(e);
                    continue;
                }
            };
            let resultados = match datos.get("results").and_then(|r| r.as_array()) {
                Some(r) if !r.is_empty() => r,
                // Con filtro de año y sin resultados: probar sin filtro.
                _ => continue,
            };
            for item in resultados.iter().take(10) {
                let id = match item.get("id").and_then(|i| i.as_i64()) {
                    Some(i) => i,
                    None => continue,
                };
                if !ids_vistos.insert(id) {
                    continue;
                }
                let titulo_c = item
                    .get(campo_titulo)
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string();
                let original = item
                    .get(campo_original)
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string();
                let anio_c = anio_de_fecha(
                    item.get(campo_fecha).and_then(|d| d.as_str()).unwrap_or(""),
                );
                let popularidad =
                    item.get("popularity").and_then(|p| p.as_f64()).unwrap_or(0.0);
                let overview: String = item
                    .get("overview")
                    .and_then(|o| o.as_str())
                    .unwrap_or("")
                    .chars()
                    .take(180)
                    .collect();
                todos.push(Candidato {
                    id,
                    media_type: media_type.to_string(),
                    titulo: titulo_c,
                    nombre_original: original,
                    anio: anio_c,
                    popularidad,
                    overview,
                    score: 0.0,
                    fiable: variante.fiable,
                });
            }
            // Esta variante ya dio resultados: no hace falta el intento sin año.
            break;
        }
        if todos.len() >= MAX_CANDIDATOS * 2 {
            break;
        }
    }

    // Sin ningún candidato y con al menos un fallo de red: no sabemos si el
    // contenido existe. Que decida el llamador (reintentar), no la cuarentena.
    if todos.is_empty() {
        if let Some(e) = error_red {
            return Err(ErrorTmdb::Red(e));
        }
    }

    for c in todos.iter_mut() {
        let mut s = similitud(titulo, &c.titulo, &c.nombre_original);
        // El año desempata homónimos: coincidir (±1, por bailes de fecha de
        // estreno) suma un poco; no coincidir cuando el archivo declara año
        // resta lo bastante para caer del umbral de auto-renombrado.
        if let (Some(a), Ok(ac)) = (anio, c.anio.parse::<i32>()) {
            if ac > 0 {
                if (ac - a as i32).abs() <= 1 {
                    s += BONUS_ANIO;
                } else {
                    s -= PENALIZACION_ANIO;
                }
            }
        }
        c.score = s.clamp(0.0, 1.0);
    }
    todos.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                b.popularidad
                    .partial_cmp(&a.popularidad)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    });
    todos.truncate(MAX_CANDIDATOS);
    Ok(todos)
}

pub fn buscar_candidatos_serie(
    client: &Client,
    titulo: &str,
    anio: Option<u32>,
    api_key: &str,
    idioma: Idioma,
) -> Result<Vec<Candidato>, ErrorTmdb> {
    buscar_candidatos(client, titulo, anio, api_key, idioma, "tv")
}

pub fn buscar_candidatos_pelicula(
    client: &Client,
    titulo: &str,
    anio: Option<u32>,
    api_key: &str,
    idioma: Idioma,
) -> Result<Vec<Candidato>, ErrorTmdb> {
    buscar_candidatos(client, titulo, anio, api_key, idioma, "movie")
}

/// Devuelve (nombre, año) de una serie a partir de su id. `NoEncontrado`
/// típicamente significa que un id cacheado ya no existe en TMDb.
pub fn buscar_nombre_serie(
    client: &Client,
    api_key: &str,
    idioma: Idioma,
    series_id: i64,
) -> Result<(String, String), ErrorTmdb> {
    let url = format!("{}/tv/{}", BASE_URL, series_id);
    let datos = get_json(
        client,
        &url,
        &[("api_key", api_key), ("language", idioma.codigo())],
    )?;
    let nombre = datos
        .get("name")
        .and_then(|n| n.as_str())
        .ok_or(ErrorTmdb::NoEncontrado)?
        .to_string();
    let anio = anio_de_fecha(
        datos
            .get("first_air_date")
            .and_then(|d| d.as_str())
            .unwrap_or(""),
    );
    Ok((nombre, anio))
}

/// Devuelve el nombre de la colección/saga de TMDb a la que pertenece una
/// película (campo `belongs_to_collection.name`), tal cual lo da TMDb, u
/// `Ok(None)` si la película no forma parte de ninguna colección.
pub fn buscar_coleccion_pelicula(
    client: &Client,
    api_key: &str,
    idioma: Idioma,
    movie_id: i64,
) -> Result<Option<String>, ErrorTmdb> {
    let url = format!("{}/movie/{}", BASE_URL, movie_id);
    let datos = match get_json(
        client,
        &url,
        &[("api_key", api_key), ("language", idioma.codigo())],
    ) {
        Ok(d) => d,
        Err(ErrorTmdb::NoEncontrado) => return Ok(None),
        Err(e) => return Err(e),
    };
    Ok(datos
        .get("belongs_to_collection")
        .and_then(|c| c.get("name"))
        .and_then(|n| n.as_str())
        .map(|s| s.to_string()))
}

/// Devuelve los nombres de TODOS los episodios de una temporada en una sola
/// llamada (`/tv/{id}/season/{n}`), como mapa `nº de episodio -> nombre`.
/// Mucho más eficiente que pedir episodio a episodio cuando llega una
/// temporada completa: ~2 llamadas por temporada en vez de 2 por capítulo.
pub fn buscar_temporada(
    client: &Client,
    api_key: &str,
    idioma: Idioma,
    series_id: i64,
    temporada: u32,
) -> Result<HashMap<u32, String>, ErrorTmdb> {
    let url = format!("{}/tv/{}/season/{}", BASE_URL, series_id, temporada);
    let datos = get_json(
        client,
        &url,
        &[("api_key", api_key), ("language", idioma.codigo())],
    )?;
    let mut episodios = HashMap::new();
    if let Some(lista) = datos.get("episodes").and_then(|e| e.as_array()) {
        for item in lista {
            let num = match item.get("episode_number").and_then(|n| n.as_u64()) {
                Some(n) => n as u32,
                None => continue,
            };
            if let Some(nombre) = item.get("name").and_then(|n| n.as_str()) {
                if !nombre.trim().is_empty() {
                    episodios.insert(num, nombre.to_string());
                }
            }
        }
    }
    Ok(episodios)
}

/// Devuelve el nombre de un episodio concreto.
pub fn buscar_nombre_episodio(
    client: &Client,
    api_key: &str,
    idioma: Idioma,
    series_id: i64,
    temporada: u32,
    episodio: u32,
) -> Option<String> {
    let url = format!(
        "{}/tv/{}/season/{}/episode/{}",
        BASE_URL, series_id, temporada, episodio
    );
    let resp = client
        .get(&url)
        .query(&[("api_key", api_key), ("language", idioma.codigo())])
        .send()
        .ok()?;
    let datos: Value = resp.json().ok()?;
    datos
        .get("name")
        .and_then(|n| n.as_str())
        .map(|s| s.to_string())
}
