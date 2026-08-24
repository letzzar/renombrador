//! Parseo y limpieza de nombres de archivo, generación de variantes de
//! búsqueda y puntuación de similitud. Lógica portada tal cual de la app
//! original (no se ha cambiado el comportamiento).

use crate::config::Idioma;
use regex::Regex;
use std::path::Path;
use std::sync::OnceLock;
use strsim::jaro_winkler;
use unicode_normalization::UnicodeNormalization;

/// Compila un regex una sola vez por proceso (los patrones son constantes y
/// `Regex::new` es costoso; antes se recompilaban con cada archivo).
macro_rules! regex_estatico {
    ($pat:expr) => {{
        static RE: OnceLock<Regex> = OnceLock::new();
        RE.get_or_init(|| Regex::new($pat).unwrap())
    }};
}

#[derive(Clone, Copy, Debug)]
pub struct EpisodioInfo {
    pub temporada: u32,
    pub episodio: u32,
}

/// Extrae el título "limpio", el año de estreno (si aparece) y, si la
/// detecta, la info de temporada/episodio a partir del nombre de archivo.
///
/// El año se devuelve **separado** del título: la API de TMDb no entiende
/// queries con el año incrustado (`"Dune 2021"` da 0 resultados), pero sí
/// acepta el parámetro `primary_release_year`/`first_air_date_year`, que es
/// donde acaba este valor. Así el año sigue sirviendo para diferenciar
/// remakes sin sabotear la búsqueda.
///
/// Sobre el original se añade limpieza de "etiquetas de release" típicas de
/// eMule/torrents (resolución, fuente, códec, audio, grupo entre corchetes…)
/// para que el título que se busca en TMDb sea el de verdad y no
/// `Dune 2021 1080p BluRay x264 GRUPO`.
pub fn extraer_info_archivo(
    nombre_archivo: &str,
) -> (String, Option<u32>, Option<EpisodioInfo>) {
    let (sin_corchetes, pos_episodio) = preparar(nombre_archivo);
    let episodio_info = pos_episodio.map(|(_, _, ep)| ep);
    let pos_episodio = pos_episodio.map(|(ini, fin, _)| (ini, fin));

    let Segmentos {
        titulo_prefijo,
        anio_prefijo,
        titulo_sufijo,
        anio_sufijo,
    } = segmentar(&sin_corchetes, pos_episodio);

    if sufijo_amplia_al_prefijo(&titulo_prefijo, &titulo_sufijo) {
        // El sufijo es el mismo título, más completo: el prefijo estaba
        // truncado. El año del prefijo sirve de reserva si el sufijo no trae.
        (titulo_sufijo, anio_sufijo.or(anio_prefijo), episodio_info)
    } else {
        // Se descarta el sufijo entero, año incluido: un "2019" dentro del
        // título del capítulo no es el año de estreno de la serie.
        (titulo_prefijo, anio_prefijo, episodio_info)
    }
}

/// Deja el nombre listo para trocear: sin extensión, en NFC, sin segmentos
/// entre corchetes, y localiza el código de episodio.
///
/// Devuelve el texto ya saneado y, si hay episodio, `(inicio, fin, info)` de su
/// código dentro de ese texto.
fn preparar(nombre_archivo: &str) -> (String, Option<(usize, usize, EpisodioInfo)>) {
    let path = Path::new(nombre_archivo);
    // Normalizar a NFC: los sistemas de archivos de macOS guardan los nombres
    // en NFD ("Á" = "A" + tilde combinante) y la búsqueda de TMDb devuelve 0
    // resultados para queries NFD. Con NFC el título funciona en todas partes.
    let nombre_sin_ext: String = path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .nfc()
        .collect();

    // Quitar segmentos entre corchetes/llaves: [YTS.MX], {eztv}, [www.web.com]…
    let re_corchetes = regex_estatico!(r"[\[\{][^\]\}]*[\]\}]");
    let sin_corchetes = re_corchetes.replace_all(&nombre_sin_ext, " ").to_string();

    let re_episodio_simple = regex_estatico!(r"\b(\d+)x(\d+)\b");
    let re_episodio_formato = regex_estatico!(r"[Ss](\d+)[Ee](\d+)");

    // Detección de episodio con guardia: descartamos temporadas absurdas
    // (p. ej. una resolución "1920x1080" no es la temporada 1920).
    let pos = re_episodio_simple
        .captures(&sin_corchetes)
        .and_then(ep_desde_captura)
        .or_else(|| {
            re_episodio_formato
                .captures(&sin_corchetes)
                .and_then(ep_desde_captura)
        })
        .map(|(ep, ini, fin)| (ini, fin, ep));

    (sin_corchetes, pos)
}

/// Los dos lados del código de episodio, ya limpios.
struct Segmentos {
    titulo_prefijo: String,
    anio_prefijo: Option<u32>,
    titulo_sufijo: String,
    anio_sufijo: Option<u32>,
}

/// Parte el nombre por el código de episodio y limpia cada mitad.
///
/// El texto de la derecha suele ser el título del capítulo (ruido), pero no
/// siempre: en `Star Trek 1x01 Star Trek Strange New Worlds (2022)` es donde
/// vive el nombre real de la serie, y en `Star Trek 1x01 Strange New Worlds
/// (2022)` es la *segunda mitad* de ese nombre.
fn segmentar(sin_corchetes: &str, pos_episodio: Option<(usize, usize)>) -> Segmentos {
    let (prefijo, sufijo) = match pos_episodio {
        Some((ini, fin)) => (&sin_corchetes[..ini], &sin_corchetes[fin..]),
        None => (sin_corchetes, ""),
    };

    // El primer token del prefijo nunca se recorta (hay películas tituladas
    // "Cam" o "Web"); el del sufijo sí, porque un sufijo que arranca con una
    // etiqueta de release es ruido puro ("Los.Simpson.12x08.WEB-DL").
    let (titulo_prefijo, anio_prefijo) = limpiar_segmento(prefijo, false);
    let (titulo_sufijo, anio_sufijo) = limpiar_segmento(sufijo, true);

    Segmentos {
        titulo_prefijo,
        anio_prefijo,
        titulo_sufijo,
        anio_sufijo,
    }
}

/// Título formado por las **dos** mitades unidas, para nombres en los que el
/// código de episodio parte el nombre de la serie por la mitad:
/// `Star Trek 2x10 Strange New Worlds (2022)` -> `Star Trek Strange New Worlds`.
///
/// No se puede decidir desde el nombre si el sufijo completa el título o es el
/// título del capítulo (`Severance 1x01 Good News About Hell`): las dos formas
/// son idénticas. Por eso esto no sustituye a `extraer_info_archivo`, solo
/// ofrece un segundo candidato que la búsqueda prueba **si la primera no
/// convence**; quien decide es TMDb, no una heurística sobre el nombre.
///
/// Devuelve `None` cuando no hay nada que unir: sin sufijo, sin prefijo, o
/// cuando el sufijo ya amplía al prefijo (ese caso ya lo resuelve
/// `extraer_info_archivo` por sí solo).
pub fn titulo_compuesto(nombre_archivo: &str) -> Option<(String, Option<u32>)> {
    let (sin_corchetes, pos_episodio) = preparar(nombre_archivo);
    let (ini, fin, _) = pos_episodio?;
    let seg = segmentar(&sin_corchetes, Some((ini, fin)));

    if seg.titulo_prefijo.is_empty() || seg.titulo_sufijo.is_empty() {
        return None;
    }
    if sufijo_amplia_al_prefijo(&seg.titulo_prefijo, &seg.titulo_sufijo) {
        return None;
    }
    Some((
        format!("{} {}", seg.titulo_prefijo, seg.titulo_sufijo),
        seg.anio_sufijo.or(seg.anio_prefijo),
    ))
}

/// El año que `extraer_info_archivo` tira a la basura junto con el sufijo,
/// para poder recuperarlo como **segundo intento** cuando la búsqueda sin él
/// no ha convencido.
///
/// El sufijo se descarta entero por una razón buena: en
/// `Los Simpson 12x08 Un tranvía llamado Marge (1991)` ese año es el de emisión
/// del capítulo, no el de estreno de la serie, y meterlo en la búsqueda
/// penalizaría a la serie correcta. Pero en `Silo 3x07 Radio (2023)` es el año
/// de la serie, y es justo lo que separa a dos homónimas.
///
/// Como desde el nombre no se puede saber cuál de los dos casos es, esto no
/// cambia lo que devuelve `extraer_info_archivo`: solo ofrece el dato para un
/// reintento que, si tampoco convence, se descarta sin efecto.
///
/// Devuelve `None` cuando el año no aporta nada nuevo: sin episodio, sin año en
/// el sufijo, cuando el prefijo ya trae el suyo (ese manda) o cuando el sufijo
/// amplía al prefijo (ahí `extraer_info_archivo` ya devuelve este año).
pub fn anio_del_sufijo(nombre_archivo: &str) -> Option<u32> {
    let (sin_corchetes, pos_episodio) = preparar(nombre_archivo);
    let (ini, fin, _) = pos_episodio?;
    let seg = segmentar(&sin_corchetes, Some((ini, fin)));

    if seg.anio_prefijo.is_some() {
        return None;
    }
    if sufijo_amplia_al_prefijo(&seg.titulo_prefijo, &seg.titulo_sufijo) {
        return None;
    }
    seg.anio_sufijo
}

/// Normaliza un trozo del nombre de archivo a texto buscable y le extrae el
/// año de estreno. Es la limpieza que antes se aplicaba solo al texto anterior
/// al código de episodio.
fn limpiar_segmento(seg: &str, cortar_en_primer_token: bool) -> (String, Option<u32>) {
    let seg = seg.trim();

    // Año entre paréntesis: "(2014)". Es la señal más fiable del año de
    // estreno, así que lo capturamos (el último si hubiera varios) y lo
    // quitamos del título para que la query a TMDb vaya limpia.
    let re_año = regex_estatico!(r"\(\s*([12]\d{3})\s*\)");
    let mut anio: Option<u32> = re_año
        .captures_iter(seg)
        .last()
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse().ok());
    let titulo_sin_metadata = re_año.replace_all(seg, " ");

    // Capa 1: limpieza no destructiva. Sustituimos solo separadores típicos
    // de nombres de release (puntos, guiones bajos, guiones) por espacios y
    // colapsamos. Conservamos diacríticos y apóstrofes para no romper títulos
    // como "La maldición de Widow's Bay".
    let re_sep = regex_estatico!(r"[\._\-]+");
    let titulo_espaciado = re_sep.replace_all(titulo_sin_metadata.as_ref(), " ");

    // Capa 1b: recortar el título en cuanto aparece una etiqueta de release.
    // Todo lo que va después (calidad, códec, grupo…) es ruido para la búsqueda.
    let titulo_recortado = recortar_en_tag_release(&titulo_espaciado, cortar_en_primer_token);

    // Quitar caracteres prohibidos en nombres de archivo (y barras varias).
    let re_invalidos = regex_estatico!(r#"[<>:"/\\|?*]"#);
    let limpio = re_invalidos.replace_all(&titulo_recortado, " ");

    // Colapsar espacios.
    let re_espacios = regex_estatico!(r"\s+");
    let mut limpio = re_espacios.replace_all(&limpio, " ").trim().to_string();

    // Año "suelto" al final del título ya recortado: "Dune 2021" -> ("Dune",
    // 2021). Solo el último token, solo si parece un año de cine y solo si no
    // deja el título vacío ("1917" a secas ES el título, no un año). Un año
    // anterior entre paréntesis tiene prioridad: "Blade Runner 2049 (2017)"
    // ya resolvió anio=2017 y aquí no se toca el "2049".
    if anio.is_none() {
        let tokens: Vec<&str> = limpio.split_whitespace().collect();
        if tokens.len() > 1 {
            if let Ok(a) = tokens[tokens.len() - 1].parse::<u32>() {
                if (1900..=2099).contains(&a) {
                    anio = Some(a);
                    limpio = tokens[..tokens.len() - 1].join(" ");
                }
            }
        }
    }

    (limpio, anio)
}

/// ¿El texto posterior al código de episodio es el mismo título del prefijo
/// pero más completo?
///
/// Es la señal que distingue `Star Trek 1x01 Star Trek Strange New Worlds`
/// (el prefijo venía truncado; manda el sufijo) de
/// `Severance 1x01 Good News About Hell` (el sufijo es el nombre del capítulo;
/// manda el prefijo). Se exige frontera de palabra para que "Star" no case con
/// "Stargate".
fn sufijo_amplia_al_prefijo(prefijo: &str, sufijo: &str) -> bool {
    if prefijo.is_empty() {
        return !sufijo.is_empty();
    }
    if sufijo.len() <= prefijo.len() {
        return false;
    }
    let p = prefijo.to_lowercase();
    let s = sufijo.to_lowercase();
    s.starts_with(&p) && s[p.len()..].starts_with(' ')
}

/// Convierte una captura `NxNN` / `SxxExx` en `(EpisodioInfo, inicio, fin)`,
/// descartando temporadas implausibles (> 50) para no confundir resoluciones
/// como `1920x1080` con un episodio.
fn ep_desde_captura(cap: regex::Captures) -> Option<(EpisodioInfo, usize, usize)> {
    let t = cap.get(1)?.as_str().parse::<u32>().ok()?;
    let e = cap.get(2)?.as_str().parse::<u32>().ok()?;
    if (1..=50).contains(&t) {
        let m = cap.get(0).unwrap();
        Some((EpisodioInfo { temporada: t, episodio: e }, m.start(), m.end()))
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
///
/// Con `desde_primer_token = false` nunca corta en el token inicial, que es lo
/// que evita vaciar el título en películas como "Cam" o "Web". Con `true` sí
/// puede vaciarlo: se usa para el texto posterior al código de episodio, donde
/// un arranque como "WEB-DL" no es un título sino ruido.
fn recortar_en_tag_release(s: &str, desde_primer_token: bool) -> String {
    let tokens: Vec<&str> = s.split_whitespace().collect();
    let inicio = if desde_primer_token { 0 } else { 1 };
    let mut corte = tokens.len();
    for (i, tok) in tokens.iter().enumerate().skip(inicio) {
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

/// Pliega diacríticos para comparar palabras sin que la tilde cuente: "días"
/// y "dias" son la misma palabra. Descompone en NFD y descarta las marcas
/// combinantes resultantes.
fn plegar_acentos(s: &str) -> String {
    s.to_lowercase()
        .nfd()
        .filter(|c| !matches!(*c as u32, 0x0300..=0x036F))
        .collect()
}

/// Qué proporción de las palabras fuertes de `query` aparece en `nombre`.
///
/// Es la señal que distingue un id de caché correcto de uno podrido, y hace el
/// trabajo que la similitud Jaro-Winkler **no** puede hacer: comparar
/// `"star trek strange new worlds"` con `"Star Trek"` da 0.86, más que
/// `"marshals"` contra `"Marshals: Una historia de Yellowstone"` (0.84), porque
/// premia el prefijo común. Ningún umbral de similitud separa esos dos casos.
/// La cobertura sí: 0.40 el podrido, 1.00 el bueno.
///
/// Se mide solo en un sentido (palabras de la query presentes en el nombre)
/// porque al nombre de TMDb le sobran subtítulos que el archivo no trae
/// ("Marshals: Una historia de Yellowstone"); lo sospechoso es lo contrario,
/// que el archivo nombre cosas que la serie no menciona.
pub fn cobertura_palabras(query: &str, nombre: &str) -> f64 {
    let palabras = palabras_fuertes(query);
    if palabras.is_empty() {
        return 1.0;
    }
    let nombre_plegado = plegar_acentos(nombre);
    let presentes = palabras
        .iter()
        .filter(|p| nombre_plegado.contains(&plegar_acentos(p)))
        .count();
    presentes as f64 / palabras.len() as f64
}

/// Una query candidata contra TMDb, con el idioma en que buscar y si el
/// resultado puede considerarse fiable para renombrar automáticamente.
#[derive(Clone, Debug)]
pub struct Variante {
    pub query: String,
    pub idioma: Idioma,
    /// `false` en las variantes "desesperadas" (una sola palabra): recuperan
    /// demasiada morralla como para auto-renombrar por encima del umbral.
    /// Sus resultados solo valen como sugerencias para revisión manual.
    pub fiable: bool,
}

/// Genera variantes de búsqueda en orden de prioridad, para ir probando
/// contra TMDb hasta encontrar algo útil.
pub fn variantes_busqueda(titulo: &str, idioma: Idioma) -> Vec<Variante> {
    let mut vs: Vec<Variante> = Vec::new();
    let base = titulo.trim().to_string();
    if base.is_empty() {
        return vs;
    }

    let agregar = |vs: &mut Vec<Variante>, q: String, l: Idioma, fiable: bool| {
        let q = q.trim().to_string();
        if q.is_empty() {
            return;
        }
        if !vs.iter().any(|v| v.query == q && v.idioma == l) {
            vs.push(Variante { query: q, idioma: l, fiable });
        }
    };

    // 1) Título completo en idioma elegido.
    agregar(&mut vs, base.clone(), idioma, true);

    // 2) Mismo título en inglés (TMDb suele tener el original en EN).
    if idioma != Idioma::EnUS {
        agregar(&mut vs, base.clone(), Idioma::EnUS, true);
    }

    // 3) Sin artículo líder.
    if let Some(sin_art) = quitar_articulo_lider(&base) {
        agregar(&mut vs, sin_art.clone(), idioma, true);
        if idioma != Idioma::EnUS {
            agregar(&mut vs, sin_art, Idioma::EnUS, true);
        }
    }

    // 4) Últimas N palabras fuertes (sufijo del título).
    let palabras = palabras_fuertes(&base);
    for n in [4usize, 3, 2] {
        if palabras.len() > n {
            let sufijo: String = palabras[palabras.len() - n..].join(" ");
            agregar(&mut vs, sufijo.clone(), idioma, true);
            if idioma != Idioma::EnUS {
                agregar(&mut vs, sufijo, Idioma::EnUS, true);
            }
        }
    }

    // 5) Primera palabra fuerte (último intento desesperado, no fiable).
    // Solo tiene sentido si el título tenía más de una palabra; si no, ya
    // está cubierto por la variante 1.
    if palabras.len() >= 2 {
        if let Some(primera) = palabras.first() {
            agregar(&mut vs, primera.clone(), idioma, false);
        }
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

/// Clave de caché para un título con año opcional. Incluir el año evita que
/// dos series homónimas de distinto año ("Doctor Who" 1963/2005) compartan
/// entrada; además mantiene compatibilidad con cachés antiguas, cuyas claves
/// se generaban con el año dentro del título.
pub fn clave_cache_titulo(titulo: &str, anio: Option<u32>) -> String {
    match anio {
        Some(a) => clave_cache(&format!("{} {}", titulo, a)),
        None => clave_cache(titulo),
    }
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
    let re_invalidos = regex_estatico!(r#"[<>:"/\\|?*]"#);
    let limpio = re_invalidos.replace_all(nombre, "");

    let re_espacios = regex_estatico!(" +");
    let limpio = re_espacios.replace_all(&limpio, " ");

    limpio.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn titulo_y_anio(n: &str) -> (String, Option<u32>) {
        let (t, a, _) = extraer_info_archivo(n);
        (t, a)
    }

    #[test]
    fn pelicula_con_tags_de_release() {
        assert_eq!(
            titulo_y_anio("Dune.2021.1080p.BluRay.x264-GROUP.mkv"),
            ("Dune".to_string(), Some(2021))
        );
        assert_eq!(
            titulo_y_anio("The.Matrix.1999.1080p.BluRay.x264.mkv"),
            ("The Matrix".to_string(), Some(1999))
        );
        assert_eq!(
            titulo_y_anio("Top.Gun.Maverick.2022.2160p.UHD.BluRay.x265-TERMINAL.mkv"),
            ("Top Gun Maverick".to_string(), Some(2022))
        );
    }

    #[test]
    fn conserva_anios_que_son_parte_del_titulo() {
        // El año de estreno se separa; el que forma parte del título se queda.
        assert_eq!(
            titulo_y_anio("Blade.Runner.2049.2017.2160p.BluRay.x265.mkv"),
            ("Blade Runner 2049".to_string(), Some(2017))
        );
        assert_eq!(
            titulo_y_anio("1917.2019.1080p.BluRay.x264.mkv"),
            ("1917".to_string(), Some(2019))
        );
        // Un título que ES un año no debe quedarse vacío.
        assert_eq!(titulo_y_anio("1917.mkv"), ("1917".to_string(), None));
    }

    #[test]
    fn quita_corchetes_y_separa_el_anio() {
        assert_eq!(
            titulo_y_anio("[YTS.MX] Interstellar (2014) [1080p].mkv"),
            ("Interstellar".to_string(), Some(2014))
        );
    }

    #[test]
    fn anio_entre_parentesis_tiene_prioridad() {
        assert_eq!(
            titulo_y_anio("Inception (2010).mkv"),
            ("Inception".to_string(), Some(2010))
        );
        assert_eq!(
            titulo_y_anio("Dune (1984) 1080p BluRay x264.mkv"),
            ("Dune".to_string(), Some(1984))
        );
        // El paréntesis fija el año; el "2049" del título no se toca.
        assert_eq!(
            titulo_y_anio("Blade Runner 2049 (2017).mkv"),
            ("Blade Runner 2049".to_string(), Some(2017))
        );
    }

    #[test]
    fn pelicula_con_titulo_acentuado_y_tags_entre_parentesis() {
        // Caso real: el año va entre paréntesis y los tags también.
        assert_eq!(
            titulo_y_anio(
                "Águilas.de.El.Cairo.(2025).(Spanish.Arabic.Subs).WEB-DL.1080p.x264-EAC3.by.xusman.(nocturniap2p).mkv"
            ),
            ("Águilas de El Cairo".to_string(), Some(2025))
        );
    }

    #[test]
    fn serie_separa_anio_del_titulo() {
        let (t, a, ep) = extraer_info_archivo("Doctor.Who.(2005).S01E01.1080p.HDTV.mkv");
        assert_eq!(t, "Doctor Who");
        assert_eq!(a, Some(2005));
        assert_eq!(ep.unwrap().temporada, 1);
        assert_eq!(ep.unwrap().episodio, 1);
    }

    #[test]
    fn series_extrae_episodio_y_titulo() {
        let (t, _, ep) = extraer_info_archivo("Severance.S01E01.1080p.WEB.mkv");
        assert_eq!(t, "Severance");
        assert_eq!(ep.unwrap().temporada, 1);
        assert_eq!(ep.unwrap().episodio, 1);

        let (t2, _, ep2) = extraer_info_archivo("Los.Simpson.12x08.WEB-DL.mkv");
        assert_eq!(t2, "Los Simpson");
        assert_eq!(ep2.unwrap().temporada, 12);
        assert_eq!(ep2.unwrap().episodio, 8);
    }

    #[test]
    fn resolucion_wxh_no_es_episodio() {
        // "1920x1080" no debe interpretarse como temporada 1920.
        let (_, _, ep) = extraer_info_archivo("Pelicula.1920x1080.x264.mkv");
        assert!(ep.is_none());
    }

    #[test]
    fn no_corta_titulos_de_una_palabra_que_son_tag() {
        // El primer token nunca se corta aunque sea una etiqueta.
        assert_eq!(
            titulo_y_anio("Cam.2018.1080p.WEBRip.mkv"),
            ("Cam".to_string(), Some(2018))
        );
    }

    // --- Regresión: el texto tras el código de episodio ya no se descarta ---

    #[test]
    fn usa_el_sufijo_cuando_amplia_el_titulo_del_prefijo() {
        // Caso real: el prefijo "Star Trek" coincidía al 100 % con la serie de
        // 1966 y el nombre verdadero quedaba detrás del "1x01".
        let (t, a, ep) =
            extraer_info_archivo("Star Trek 1x01 Star Trek Strange New Worlds (2022).mkv");
        assert_eq!(t, "Star Trek Strange New Worlds");
        assert_eq!(a, Some(2022));
        assert_eq!(ep.unwrap().temporada, 1);
        assert_eq!(ep.unwrap().episodio, 1);
    }

    #[test]
    fn el_sufijo_no_pisa_un_titulo_de_episodio_normal() {
        // Aquí el sufijo es el nombre del capítulo: no empieza por el prefijo,
        // así que manda el prefijo, como siempre.
        let (t, _, _) = extraer_info_archivo("Severance.1x01.Good.News.About.Hell.1080p.WEB.mkv");
        assert_eq!(t, "Severance");

        let (t2, _, _) = extraer_info_archivo(
            "Star.Trek.Strange.New.Worlds.1x01.Nuevos.mundos.extraños.(Spanish.English.Subs).WEB-DL.1080p.x264-EAC3.by.Bryan_122.mkv",
        );
        assert_eq!(t2, "Star Trek Strange New Worlds");
    }

    #[test]
    fn el_anio_del_titulo_del_capitulo_no_se_toma_como_anio_de_la_serie() {
        // "2019" pertenece al nombre del episodio, que se descarta entero.
        let (t, a, _) = extraer_info_archivo("Serie.1x01.Un.capitulo.de.2019.WEB-DL.mkv");
        assert_eq!(t, "Serie");
        assert_eq!(a, None);
    }

    #[test]
    fn sufijo_de_puro_ruido_no_sustituye_al_prefijo() {
        let (t, _, _) = extraer_info_archivo("Los.Simpson.12x08.WEB-DL.x264.mkv");
        assert_eq!(t, "Los Simpson");
    }

    #[test]
    fn sin_prefijo_se_usa_el_sufijo() {
        let (t, a, _) = extraer_info_archivo("1x01 Star Trek Strange New Worlds (2022).mkv");
        assert_eq!(t, "Star Trek Strange New Worlds");
        assert_eq!(a, Some(2022));
    }

    #[test]
    fn el_codigo_de_episodio_puede_partir_el_titulo_por_la_mitad() {
        // Caso real: 20 episodios de Strange New Worlds nombrados asi acababan
        // en `_revisar`, porque el prefijo suelto ("Star Trek") empata con media
        // docena de series y el sufijo se descartaba entero.
        let n = "Star Trek 2x10 Strange New Worlds (2022).mkv";
        // El titulo principal sigue siendo el prefijo: desde el nombre no se
        // puede saber si el sufijo completa el titulo o nombra el capitulo.
        let (t, a, _) = extraer_info_archivo(n);
        assert_eq!(t, "Star Trek");
        assert_eq!(a, None);
        // Y el compuesto ofrece la otra lectura, con su anio.
        assert_eq!(
            titulo_compuesto(n),
            Some(("Star Trek Strange New Worlds".to_string(), Some(2022)))
        );
    }

    #[test]
    fn el_compuesto_tambien_se_ofrece_para_un_titulo_de_capitulo() {
        // No se puede distinguir del caso anterior mirando solo el nombre, asi
        // que aqui tambien se ofrece; lo descarta la busqueda, no el parser:
        // "Severance" ya gana por si solo y el plan B ni llega a probarse.
        assert_eq!(
            titulo_compuesto("Severance 1x01 Good News About Hell.mkv"),
            Some(("Severance Good News About Hell".to_string(), None))
        );
    }

    #[test]
    fn no_hay_compuesto_cuando_no_hay_nada_que_unir() {
        // Sin episodio, sin sufijo, y sufijo de puro ruido (se limpia a vacio).
        assert_eq!(titulo_compuesto("Dune (2021).mkv"), None);
        assert_eq!(titulo_compuesto("Los Simpson 12x08.mkv"), None);
        assert_eq!(titulo_compuesto("Los.Simpson.12x08.WEB-DL.x264.mkv"), None);
        // El sufijo que amplia al prefijo ya lo resuelve extraer_info_archivo.
        assert_eq!(
            titulo_compuesto("Star Trek 1x01 Star Trek Strange New Worlds (2022).mkv"),
            None
        );
    }

    #[test]
    fn el_anio_del_sufijo_queda_disponible_para_un_segundo_intento() {
        // Caso real de `Silo 3x07 Radio (2023)`: el (2023) se descarta con el
        // sufijo (podria ser el anio de emision del capitulo), pero se guarda
        // para reintentar la busqueda si el titulo solo no convence.
        let n = "Silo 3x07 Radio (2023).mkv";
        assert_eq!(titulo_y_anio(n), ("Silo".to_string(), None));
        assert_eq!(anio_del_sufijo(n), Some(2023));
    }

    #[test]
    fn no_hay_anio_de_sufijo_que_recuperar() {
        // Sin anio detras del codigo de episodio.
        assert_eq!(anio_del_sufijo("Silo.3x08.Plaga.gris.WEB-DL.mkv"), None);
        // Sin episodio: no hay sufijo del que hablar.
        assert_eq!(anio_del_sufijo("Dune (2021).mkv"), None);
        // El prefijo ya trae su anio: ese manda y el reintento no aporta.
        assert_eq!(anio_del_sufijo("Silo (2023) 3x07 Radio (1999).mkv"), None);
        // El sufijo amplia al prefijo: su anio ya lo devuelve el parser.
        assert_eq!(
            anio_del_sufijo("Star Trek 1x01 Star Trek Strange New Worlds (2022).mkv"),
            None
        );
    }

    #[test]
    fn normaliza_nfd_a_nfc() {
        // Nombre como lo guarda macOS: "Á" descompuesta (A + U+0301).
        let nfd = "A\u{0301}guilas.de.El.Cairo.(2025).WEB-DL.mkv";
        let (t, a, _) = extraer_info_archivo(nfd);
        // El título resultante debe ser NFC ("Á" precompuesta, U+00C1).
        assert_eq!(t, "\u{c1}guilas de El Cairo");
        assert_eq!(a, Some(2025));
    }

    #[test]
    fn variante_de_una_palabra_no_es_fiable() {
        let vs = variantes_busqueda("Águilas de El Cairo", Idioma::EsES);
        // La primera variante (título completo) es fiable.
        assert!(vs[0].fiable);
        // La última (palabra suelta "águilas") no lo es.
        let desesperada = vs.iter().find(|v| v.query == "águilas").unwrap();
        assert!(!desesperada.fiable);
        // Un título de una sola palabra no genera variante desesperada.
        let vs1 = variantes_busqueda("Severance", Idioma::EsES);
        assert!(vs1.iter().all(|v| v.fiable));
    }
}
