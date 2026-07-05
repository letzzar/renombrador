//! Llamadas a la API de TMDb. Lógica portada de la app original.

use crate::config::Idioma;
use crate::parse::{similitud, variantes_busqueda};
use reqwest::blocking::Client;
use serde_json::Value;

pub const BASE_URL: &str = "https://api.themoviedb.org/3";

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
}

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
fn buscar_candidatos(
    client: &Client,
    titulo: &str,
    api_key: &str,
    idioma: Idioma,
    media_type: &str, // "tv" o "movie"
) -> Vec<Candidato> {
    let (endpoint, campo_titulo, campo_original, campo_fecha) = if media_type == "tv" {
        ("search/tv", "name", "original_name", "first_air_date")
    } else {
        ("search/movie", "title", "original_title", "release_date")
    };
    let url = format!("{}/{}", BASE_URL, endpoint);
    let variantes = variantes_busqueda(titulo, idioma);
    let mut todos: Vec<Candidato> = Vec::new();
    let mut ids_vistos: std::collections::HashSet<i64> = std::collections::HashSet::new();

    for (query, lang) in &variantes {
        let resp = client
            .get(&url)
            .query(&[
                ("api_key", api_key),
                ("query", query),
                ("language", lang.codigo()),
            ])
            .send();
        if let Ok(r) = resp {
            if let Ok(datos) = r.json::<Value>() {
                if let Some(resultados) = datos.get("results").and_then(|r| r.as_array()) {
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
                        let anio = anio_de_fecha(
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
                            anio,
                            popularidad,
                            overview,
                            score: 0.0,
                        });
                    }
                }
            }
        }
        if todos.len() >= MAX_CANDIDATOS * 2 {
            break;
        }
    }

    for c in todos.iter_mut() {
        c.score = similitud(titulo, &c.titulo, &c.nombre_original);
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
    todos
}

pub fn buscar_candidatos_serie(
    client: &Client,
    titulo: &str,
    api_key: &str,
    idioma: Idioma,
) -> Vec<Candidato> {
    buscar_candidatos(client, titulo, api_key, idioma, "tv")
}

pub fn buscar_candidatos_pelicula(
    client: &Client,
    titulo: &str,
    api_key: &str,
    idioma: Idioma,
) -> Vec<Candidato> {
    buscar_candidatos(client, titulo, api_key, idioma, "movie")
}

/// Devuelve (nombre, año) de una serie a partir de su id.
pub fn buscar_nombre_serie(
    client: &Client,
    api_key: &str,
    idioma: Idioma,
    series_id: i64,
) -> Option<(String, String)> {
    let url = format!("{}/tv/{}", BASE_URL, series_id);
    let resp = client
        .get(&url)
        .query(&[("api_key", api_key), ("language", idioma.codigo())])
        .send()
        .ok()?;
    let datos: Value = resp.json().ok()?;
    let nombre = datos.get("name").and_then(|n| n.as_str())?.to_string();
    let anio = anio_de_fecha(
        datos
            .get("first_air_date")
            .and_then(|d| d.as_str())
            .unwrap_or(""),
    );
    Some((nombre, anio))
}

/// Devuelve el nombre de la colección/saga de TMDb a la que pertenece una
/// película (campo `belongs_to_collection.name`), tal cual lo da TMDb, o `None`
/// si la película no forma parte de ninguna colección.
pub fn buscar_coleccion_pelicula(
    client: &Client,
    api_key: &str,
    idioma: Idioma,
    movie_id: i64,
) -> Option<String> {
    let url = format!("{}/movie/{}", BASE_URL, movie_id);
    let resp = client
        .get(&url)
        .query(&[("api_key", api_key), ("language", idioma.codigo())])
        .send()
        .ok()?;
    let datos: Value = resp.json().ok()?;
    datos
        .get("belongs_to_collection")?
        .get("name")?
        .as_str()
        .map(|s| s.to_string())
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
