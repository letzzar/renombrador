//! Parseo y limpieza de nombres de archivo, generación de variantes de
//! búsqueda y puntuación de similitud. Lógica portada tal cual de la app
//! original (no se ha cambiado el comportamiento).

use crate::config::Idioma;
use regex::Regex;
use std::path::Path;
use strsim::jaro_winkler;

#[derive(Clone, Copy, Debug)]
pub struct EpisodioInfo {
    pub temporada: u32,
    pub episodio: u32,
}

/// Extrae el título "limpio" y, si lo detecta, la info de temporada/episodio
/// a partir del nombre de archivo.
///
/// Sobre el original se añade limpieza de "etiquetas de release" típicas de
/// eMule/torrents (resolución, fuente, códec, audio, grupo entre corchetes…)
/// para que el título que se busca en TMDb sea el de verdad y no
/// `Dune 2021 1080p BluRay x264 GRUPO`.
pub fn extraer_info_archivo(nombre_archivo: &str) -> (String, Option<EpisodioInfo>) {
    let path = Path::new(nombre_archivo);
    let nombre_sin_ext = path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    // Quitar segmentos entre corchetes/llaves: [YTS.MX], {eztv}, [www.web.com]…
    let re_corchetes = Regex::new(r"[\[\{][^\]\}]*[\]\}]").unwrap();
    let sin_corchetes = re_corchetes.replace_all(&nombre_sin_ext, " ").to_string();

    let re_episodio_simple = Regex::new(r"\b(\d+)x(\d+)\b").unwrap();
    let re_episodio_formato = Regex::new(r"[Ss](\d+)[Ee](\d+)").unwrap();

    // Detección de episodio con guardia: descartamos temporadas absurdas
    // (p. ej. una resolución "1920x1080" no es la temporada 1920).
    let (episodio_info, pos_episodio) = match re_episodio_simple
        .captures(&sin_corchetes)
        .and_then(ep_desde_captura)
        .or_else(|| {
            re_episodio_formato
                .captures(&sin_corchetes)
                .and_then(ep_desde_captura)
        }) {
        Some((ep, pos)) => (Some(ep), Some(pos)),
        None => (None, None),
    };

    let titulo_antes_episodio = if let Some(pos) = pos_episodio {
        sin_corchetes[..pos].trim()
    } else {
        &sin_corchetes
    };

    // Conservamos el año aunque venga entre paréntesis: "(2014)" -> " 2014 ".
    // El año es útil para diferenciar remakes ("Dune (1984)" vs "Dune (2021)")
    // y series ("Doctor Who (2005)"), así que NO lo eliminamos; las etiquetas
    // de release que pudieran ir después se recortan luego.
    let re_año = Regex::new(r"\(\s*([12]\d{3})\s*\)").unwrap();
    let titulo_sin_metadata = re_año.replace_all(titulo_antes_episodio, " $1 ");

    // Capa 1: limpieza no destructiva. Sustituimos solo separadores típicos
    // de nombres de release (puntos, guiones bajos, guiones) por espacios y
    // colapsamos. Conservamos diacríticos y apóstrofes para no romper títulos
    // como "La maldición de Widow's Bay".
    let re_sep = Regex::new(r"[\._\-]+").unwrap();
    let titulo_espaciado = re_sep.replace_all(titulo_sin_metadata.as_ref(), " ");

    // Capa 1b: recortar el título en cuanto aparece una etiqueta de release.
    // Todo lo que va después (calidad, códec, grupo…) es ruido para la búsqueda.
    let titulo_recortado = recortar_en_tag_release(&titulo_espaciado);

    // Quitar caracteres prohibidos en nombres de archivo (y barras varias).
    let re_invalidos = Regex::new(r#"[<>:"/\\|?*]"#).unwrap();
    let limpio = re_invalidos.replace_all(&titulo_recortado, " ");

    // Colapsar espacios.
    let re_espacios = Regex::new(r"\s+").unwrap();
    let limpio = re_espacios.replace_all(&limpio, " ").trim().to_string();

    (limpio, episodio_info)
}

/// Convierte una captura `NxNN` / `SxxExx` en `(EpisodioInfo, posición)`,
/// descartando temporadas implausibles (> 50) para no confundir resoluciones
/// como `1920x1080` con un episodio.
fn ep_desde_captura(cap: regex::Captures) -> Option<(EpisodioInfo, usize)> {
    let t = cap.get(1)?.as_str().parse::<u32>().ok()?;
    let e = cap.get(2)?.as_str().parse::<u32>().ok()?;
    if (1..=50).contains(&t) {
        Some((EpisodioInfo { temporada: t, episodio: e }, cap.get(0).unwrap().start()))
    } else {
        None
    }
}

/// Etiquetas de release que marcan el fin del título (fuente, códec, audio,
/// HDR, edición, grupos/sites conocidos). En minúsculas y sin puntuación.
/// Se omiten a propósito tokens muy cortos y ambiguos (ts, tc, cam, scr, dv…)
/// que podrían formar parte de un título real.
const TAGS_RELEASE: &[&str] = &[
    // fuentes
    "bluray", "bdrip", "brrip", "brip", "bdremux", "remux", "webrip", "webdl", "web", "hdtv",
    "pdtv", "dvdrip", "dvdscr", "dvd", "dvd5", "dvd9", "hdrip", "hdcam", "camrip", "telesync",
    "telecine", "hdts", "satrip", "vhsrip", "tvrip", "uhdbd", "bdmux",
    // resoluciones nombradas
    "4k", "2k", "uhd", "fullhd",
    // códecs
    "x264", "x265", "h264", "h265", "hevc", "avc", "xvid", "divx", "av1", "vp9", "mpeg2", "mpeg4",
    // audio
    "aac", "ac3", "eac3", "dts", "dtshd", "truehd", "atmos", "flac", "mp3", "ddp", "dd5", "ddp5",
    "lpcm", "opus",
    // hdr / profundidad de bits
    "hdr", "hdr10", "hdr10+", "dolbyvision", "sdr", "10bit", "8bit", "hi10p", "hi10",
    // ediciones / flags
    "proper", "repack", "internal", "limited", "extended", "unrated", "uncut", "remastered",
    "remaster", "directors", "theatrical", "imax", "criterion", "multi", "dual", "dualaudio",
    "subs", "subbed", "dubbed", "vose", "vos", "complete", "retail", "custom", "korsub", "hdlight",
    // idiomas frecuentes en releases
    "castellano", "latino", "spanish", "espanol", "english", "ingles", "subtitulado", "multisubs",
    // grupos / sites habituales
    "yify", "yts", "rarbg", "eztv", "ettv", "fgt", "evo", "sparks", "ntg", "tigole", "galaxyrg",
    "mkvcage", "ion10", "shitrips",
];

/// ¿El token es una resolución tipo `480p`, `720p`, `1080p`, `1080i`, `2160p`?
fn es_resolucion(t: &str) -> bool {
    let n = t.len();
    if !(3..=5).contains(&n) {
        return false;
    }
    let bytes = t.as_bytes();
    let ultimo = bytes[n - 1];
    if ultimo != b'p' && ultimo != b'i' {
        return false;
    }
    t[..n - 1].bytes().all(|b| b.is_ascii_digit())
}

/// ¿Este token marca el comienzo de las etiquetas de release?
fn es_tag_release(token: &str) -> bool {
    let limpio: String = token
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '+')
        .collect();
    if limpio.is_empty() {
        return false;
    }
    es_resolucion(&limpio) || TAGS_RELEASE.contains(&limpio.as_str())
}

/// Devuelve el título recortado justo antes de la primera etiqueta de release.
/// Nunca corta en el primer token (evita dejar el título vacío en pelis como
/// "Cam" o "Web") ni cuando no hay ninguna etiqueta.
fn recortar_en_tag_release(s: &str) -> String {
    let tokens: Vec<&str> = s.split_whitespace().collect();
    let mut corte = tokens.len();
    for (i, tok) in tokens.iter().enumerate().skip(1) {
        if es_tag_release(tok) {
            corte = i;
            break;
        }
    }
    tokens[..corte].join(" ")
}

/// Quita un artículo líder ("la", "el", "the", "le"...) si existe.
pub fn quitar_articulo_lider(s: &str) -> Option<String> {
    const ARTICULOS: &[&str] = &[
        "la", "el", "los", "las", "un", "una", "unos", "unas", "the", "a", "an", "le", "les",
        "il", "lo", "gli", "der", "die", "das", "den", "o", "os", "as", "uma", "um",
    ];
    let lower = s.to_lowercase();
    for art in ARTICULOS {
        let prefijo = format!("{} ", art);
        if lower.starts_with(&prefijo) {
            return Some(s[prefijo.len()..].trim().to_string());
        }
    }
    None
}

/// Devuelve las palabras del título excluyendo stopwords cortas (de, la, del...).
pub fn palabras_fuertes(s: &str) -> Vec<String> {
    const STOPWORDS: &[&str] = &[
        "de", "del", "la", "el", "los", "las", "un", "una", "y", "o", "a", "en", "of", "the",
        "and", "or", "to", "for", "le", "les", "du", "des", "et", "il", "la", "lo", "il",
    ];
    s.to_lowercase()
        .split_whitespace()
        .filter(|w| !STOPWORDS.contains(w))
        .filter(|w| w.chars().any(|c| c.is_alphanumeric()))
        .map(|w| w.to_string())
        .collect()
}

/// Genera variantes de búsqueda en orden de prioridad. Cada variante es un par
/// (query, idioma) para ir probando contra TMDb hasta encontrar algo útil.
pub fn variantes_busqueda(titulo: &str, idioma: Idioma) -> Vec<(String, Idioma)> {
    let mut vs: Vec<(String, Idioma)> = Vec::new();
    let base = titulo.trim().to_string();
    if base.is_empty() {
        return vs;
    }

    let agregar = |vs: &mut Vec<(String, Idioma)>, q: String, l: Idioma| {
        let q = q.trim().to_string();
        if q.is_empty() {
            return;
        }
        if !vs.iter().any(|(qq, ll)| qq == &q && *ll == l) {
            vs.push((q, l));
        }
    };

    // 1) Título completo en idioma elegido.
    agregar(&mut vs, base.clone(), idioma);

    // 2) Mismo título en inglés (TMDb suele tener el original en EN).
    if idioma != Idioma::EnUS {
        agregar(&mut vs, base.clone(), Idioma::EnUS);
    }

    // 3) Sin artículo líder.
    if let Some(sin_art) = quitar_articulo_lider(&base) {
        agregar(&mut vs, sin_art.clone(), idioma);
        if idioma != Idioma::EnUS {
            agregar(&mut vs, sin_art, Idioma::EnUS);
        }
    }

    // 4) Últimas N palabras fuertes (sufijo del título).
    let palabras = palabras_fuertes(&base);
    for n in [4usize, 3, 2] {
        if palabras.len() > n {
            let sufijo: String = palabras[palabras.len() - n..].join(" ");
            agregar(&mut vs, sufijo.clone(), idioma);
            if idioma != Idioma::EnUS {
                agregar(&mut vs, sufijo, Idioma::EnUS);
            }
        }
    }

    // 5) Primera palabra fuerte (último intento desesperado).
    if let Some(primera) = palabras.first() {
        agregar(&mut vs, primera.clone(), idioma);
    }

    vs
}

/// Calcula similitud Jaro-Winkler comparando la query contra el mejor de
/// (titulo, nombre_original). Usamos minúsculas para que diacríticos y
/// mayúsculas no penalicen.
pub fn similitud(query: &str, titulo: &str, nombre_original: &str) -> f64 {
    let q = query.to_lowercase();
    let a = jaro_winkler(&q, &titulo.to_lowercase());
    let b = jaro_winkler(&q, &nombre_original.to_lowercase());
    a.max(b)
}

/// Clave de caché: título normalizado (lower + colapso de espacios). Se usa
/// como key del caché de series y para agrupar archivos del mismo lote.
pub fn clave_cache(titulo: &str) -> String {
    titulo
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Quita caracteres inválidos para nombres de archivo y colapsa espacios.
pub fn limpiar_nombre_archivo(nombre: &str) -> String {
    let re_invalidos = Regex::new(r#"[<>:"/\\|?*]"#).unwrap();
    let limpio = re_invalidos.replace_all(nombre, "");

    let re_espacios = Regex::new(" +").unwrap();
    let limpio = re_espacios.replace_all(&limpio, " ");

    limpio.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn titulo(n: &str) -> String {
        extraer_info_archivo(n).0
    }

    #[test]
    fn pelicula_con_tags_de_release() {
        assert_eq!(titulo("Dune.2021.1080p.BluRay.x264-GROUP.mkv"), "Dune 2021");
        assert_eq!(titulo("The.Matrix.1999.1080p.BluRay.x264.mkv"), "The Matrix 1999");
        assert_eq!(
            titulo("Top.Gun.Maverick.2022.2160p.UHD.BluRay.x265-TERMINAL.mkv"),
            "Top Gun Maverick 2022"
        );
    }

    #[test]
    fn conserva_anios_que_son_parte_del_titulo() {
        // No debe cortar en el año cuando forma parte del título.
        assert_eq!(
            titulo("Blade.Runner.2049.2017.2160p.BluRay.x265.mkv"),
            "Blade Runner 2049 2017"
        );
        assert_eq!(titulo("1917.2019.1080p.BluRay.x264.mkv"), "1917 2019");
    }

    #[test]
    fn quita_corchetes_pero_conserva_el_anio() {
        assert_eq!(
            titulo("[YTS.MX] Interstellar (2014) [1080p].mkv"),
            "Interstellar 2014"
        );
    }

    #[test]
    fn conserva_anio_entre_parentesis() {
        // El año entre paréntesis debe conservarse (diferencia remakes).
        assert_eq!(titulo("Inception (2010).mkv"), "Inception 2010");
        assert_eq!(
            titulo("Dune (1984) 1080p BluRay x264.mkv"),
            "Dune 1984"
        );
    }

    #[test]
    fn serie_conserva_anio_en_el_titulo() {
        let (t, ep) = extraer_info_archivo("Doctor.Who.(2005).S01E01.1080p.HDTV.mkv");
        assert_eq!(t, "Doctor Who 2005");
        assert_eq!(ep.unwrap().temporada, 1);
        assert_eq!(ep.unwrap().episodio, 1);
    }

    #[test]
    fn series_extrae_episodio_y_titulo() {
        let (t, ep) = extraer_info_archivo("Severance.S01E01.1080p.WEB.mkv");
        assert_eq!(t, "Severance");
        assert_eq!(ep.unwrap().temporada, 1);
        assert_eq!(ep.unwrap().episodio, 1);

        let (t2, ep2) = extraer_info_archivo("Los.Simpson.12x08.WEB-DL.mkv");
        assert_eq!(t2, "Los Simpson");
        assert_eq!(ep2.unwrap().temporada, 12);
        assert_eq!(ep2.unwrap().episodio, 8);
    }

    #[test]
    fn resolucion_wxh_no_es_episodio() {
        // "1920x1080" no debe interpretarse como temporada 1920.
        let (_, ep) = extraer_info_archivo("Pelicula.1920x1080.x264.mkv");
        assert!(ep.is_none());
    }

    #[test]
    fn no_corta_titulos_de_una_palabra_que_son_tag() {
        // El primer token nunca se corta aunque sea una etiqueta.
        assert_eq!(titulo("Cam.2018.1080p.WEBRip.mkv"), "Cam 2018");
    }
}
