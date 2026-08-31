//! Renombrador TMDb — servicio headless para Linux/Docker.
//!
//! Vigila una carpeta de descargas (sondeo periódico), espera a que cada
//! archivo de vídeo termine de copiarse, lo identifica en TMDb reutilizando la
//! lógica de la app de escritorio y lo renombra y mueve a las carpetas de
//! películas o series. Toda la configuración se pasa por variables de entorno.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::thread;
use std::time::{Duration, SystemTime};

use reqwest::blocking::Client;
use walkdir::WalkDir;

use renombrador::cache::SeriesCache;
use renombrador::config::Idioma;
use renombrador::mover::{borrar_dir_vacio_bajo, crear_dirs, descripcion_permisos};
use renombrador::organizer::{procesar_archivo, AccionDudoso, Opciones, Resultado};
use renombrador::EXTENSIONES_VALIDAS;

/// Parámetros del servicio leídos del entorno.
struct Entorno {
    opts: Opciones,
    watch_dir: PathBuf,
    cache_path: PathBuf,
    intervalo: Duration,
    estable: Duration,
    min_bytes: u64,
    limpiar_vacios: bool,
}

fn log(nivel: &str, msg: impl AsRef<str>) {
    println!("[{}] {}", nivel, msg.as_ref());
    let _ = std::io::stdout().flush();
}

fn env_str(clave: &str, def: &str) -> String {
    std::env::var(clave)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| def.to_string())
}

fn env_parse<T: FromStr>(clave: &str, def: T) -> T {
    std::env::var(clave)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(def)
}

fn env_bool(clave: &str, def: bool) -> bool {
    match std::env::var(clave) {
        Ok(v) => matches!(
            v.trim().to_lowercase().as_str(),
            "1" | "true" | "yes" | "si" | "sí" | "on"
        ),
        Err(_) => def,
    }
}

fn cargar_entorno() -> Result<Entorno, String> {
    let api_key = std::env::var("TMDB_API_KEY")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            "falta TMDB_API_KEY (defínela en .env o en docker-compose.yml)".to_string()
        })?;

    let lang_raw = env_str("TMDB_LANGUAGE", "es-ES");
    let idioma = Idioma::from_codigo(&lang_raw).unwrap_or_else(|| {
        log(
            "WARN",
            format!("idioma '{}' no reconocido; usando es-ES", lang_raw),
        );
        Idioma::EsES
    });

    let watch_dir = PathBuf::from(env_str("WATCH_DIR", "/descargas"));
    let dir_peliculas = PathBuf::from(env_str("MOVIES_DIR", "/peliculas"));
    let dir_series = PathBuf::from(env_str("SERIES_DIR", "/series"));
    let dir_revisar = match std::env::var("REVIEW_DIR").ok().filter(|s| !s.trim().is_empty()) {
        Some(r) => PathBuf::from(r),
        None => watch_dir.join("_revisar"),
    };

    // 1x05 (por defecto) vs S01E05.
    let formato_raw = env_str("EPISODE_FORMAT", "1x05").to_lowercase();
    let formato_1x05 = !matches!(formato_raw.as_str(), "s01e05" | "sxxexx" | "s00e00");

    let umbral = env_parse::<f64>("MATCH_THRESHOLD", renombrador::UMBRAL_AUTO);
    let anidado = env_bool("NESTED", true);
    let usar_colecciones = env_bool("USE_COLLECTIONS", true);
    // Formato del año: "parens" => Título (2021) [estándar Plex]; "brackets" => Título [2021].
    let anio_corchetes = matches!(
        env_str("YEAR_FORMAT", "parens").to_lowercase().as_str(),
        "brackets" | "corchetes" | "[]"
    );

    let forzar_series = match env_str("FORCE_MODE", "auto").to_lowercase().as_str() {
        "series" | "tv" | "serie" => Some(true),
        "movies" | "movie" | "peliculas" | "películas" | "pelicula" => Some(false),
        _ => None, // auto
    };

    let accion_dudoso = match env_str("ON_UNCERTAIN", "revisar").to_lowercase().as_str() {
        "dejar" | "leave" | "skip" => AccionDudoso::Dejar,
        "forzar" | "force" => AccionDudoso::Forzar,
        _ => AccionDudoso::Revisar,
    };

    let intervalo = Duration::from_secs(env_parse::<u64>("POLL_INTERVAL", 30).max(1));
    let estable = Duration::from_secs(env_parse::<u64>("STABLE_SECS", 60));
    let min_bytes = env_parse::<u64>("MIN_FILE_MB", 50).saturating_mul(1024 * 1024);
    let limpiar_vacios = env_bool("CLEAN_EMPTY_DIRS", false);
    let dry_run = env_bool("DRY_RUN", false);

    let cache_path = PathBuf::from(env_str("CACHE_FILE", "/config/cache.json"));

    Ok(Entorno {
        opts: Opciones {
            api_key,
            idioma,
            dir_peliculas,
            dir_series,
            dir_revisar,
            formato_1x05,
            umbral,
            anidado,
            forzar_series,
            accion_dudoso,
            usar_colecciones,
            anio_corchetes,
            dry_run,
        },
        watch_dir,
        cache_path,
        intervalo,
        estable,
        min_bytes,
        limpiar_vacios,
    })
}

fn enmascarar(api_key: &str) -> String {
    let n = api_key.chars().count();
    if n <= 4 {
        "****".to_string()
    } else {
        let ultimos: String = api_key.chars().skip(n - 4).collect();
        format!("****{}", ultimos)
    }
}

fn banner(env: &Entorno) {
    let o = &env.opts;
    log("INFO", "Renombrador TMDb (servicio) iniciado.");
    log("INFO", format!("  API key:        {}", enmascarar(&o.api_key)));
    log("INFO", format!("  Idioma:         {}", o.idioma.codigo()));
    log("INFO", format!("  Descargas:      {}", env.watch_dir.display()));
    log("INFO", format!("  Películas:      {}", o.dir_peliculas.display()));
    log("INFO", format!("  Series:         {}", o.dir_series.display()));
    log("INFO", format!("  Revisar:        {}", o.dir_revisar.display()));
    log(
        "INFO",
        format!(
            "  Estructura:     {}",
            if o.anidado { "anidada (Plex/Jellyfin)" } else { "plana" }
        ),
    );
    log(
        "INFO",
        format!("  Formato ep.:    {}", if o.formato_1x05 { "1x05" } else { "S01E05" }),
    );
    log("INFO", format!("  Umbral match:   {:.2}", o.umbral));
    log(
        "INFO",
        format!(
            "  Modo:           {}",
            match o.forzar_series {
                Some(true) => "forzar series",
                Some(false) => "forzar películas",
                None => "autodetectar",
            }
        ),
    );
    log(
        "INFO",
        format!(
            "  Match dudoso:   {}",
            match o.accion_dudoso {
                AccionDudoso::Revisar => "mover a _revisar",
                AccionDudoso::Dejar => "dejar en descargas",
                AccionDudoso::Forzar => "forzar mejor candidato",
            }
        ),
    );
    log("INFO", format!("  Colecciones:    {}", o.usar_colecciones));
    if let Some(permisos) = descripcion_permisos() {
        log("INFO", format!("  Permisos:       {}", permisos));
    }
    log(
        "INFO",
        format!(
            "  Año:            {}",
            if o.anio_corchetes { "[2021]" } else { "(2021)" }
        ),
    );
    log(
        "INFO",
        format!(
            "  Sondeo:         cada {}s · estable tras {}s · mínimo {} MB",
            env.intervalo.as_secs(),
            env.estable.as_secs(),
            env.min_bytes / (1024 * 1024)
        ),
    );
    if o.dry_run {
        log(
            "WARN",
            "  DRY RUN:        activado — solo se registra lo que se HARÍA; no se mueve nada",
        );
    }
}

fn es_video(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(e) => EXTENSIONES_VALIDAS.contains(&e.to_lowercase().as_str()),
        None => false,
    }
}

/// Edad del archivo según su fecha de modificación.
fn edad_archivo(meta: &std::fs::Metadata) -> Duration {
    meta.modified()
        .ok()
        .and_then(|m| SystemTime::now().duration_since(m).ok())
        .unwrap_or(Duration::ZERO)
}

fn main() {
    let env = match cargar_entorno() {
        Ok(e) => e,
        Err(e) => {
            log("ERROR", e);
            std::process::exit(1);
        }
    };

    // Aseguramos que existan las carpetas (las de destino y revisión).
    for dir in [
        &env.watch_dir,
        &env.opts.dir_peliculas,
        &env.opts.dir_series,
        &env.opts.dir_revisar,
    ] {
        if let Err(e) = crear_dirs(dir) {
            log("WARN", format!("no se pudo crear '{}': {}", dir.display(), e));
        }
    }

    banner(&env);

    let client = match Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            log("ERROR", format!("no se pudo crear el cliente HTTP: {}", e));
            std::process::exit(1);
        }
    };

    let mut cache = SeriesCache::cargar(env.cache_path.clone());
    if !cache.is_empty() {
        log("INFO", format!("Caché de series cargada: {} entradas.", cache.len()));
    }

    bucle(&env, &client, &mut cache);
}

fn bucle(env: &Entorno, client: &Client, cache: &mut SeriesCache) {
    // Tamaño visto en el sondeo anterior, por ruta. Solo procesamos cuando el
    // tamaño no cambió entre dos sondeos y la mtime es lo bastante antigua.
    let mut tamano_previo: HashMap<PathBuf, u64> = HashMap::new();
    // Archivos que dejamos en su sitio (sin resultados / error / dejar): no se
    // reintentan ni se vuelven a registrar mientras no cambien.
    let mut sin_accion: HashSet<PathBuf> = HashSet::new();
    // Errores transitorios de red/TMDb: (nº de intentos, no reintentar antes
    // de). Backoff exponencial con techo para no machacar TMDb en una caída.
    let mut reintentos: HashMap<PathBuf, (u32, SystemTime)> = HashMap::new();

    loop {
        for entrada in WalkDir::new(&env.watch_dir)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let meta = match entrada.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if !meta.is_file() {
                continue;
            }
            let path = entrada.into_path();

            // No tocar lo que ya está en carpetas gestionadas.
            if path.starts_with(&env.opts.dir_revisar)
                || path.starts_with(&env.opts.dir_peliculas)
                || path.starts_with(&env.opts.dir_series)
            {
                continue;
            }
            // Ignorar ocultos/parciales y lo que no sea vídeo.
            let nombre = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            if nombre.starts_with('.') || !es_video(&path) {
                continue;
            }

            let size = meta.len();
            if size < env.min_bytes {
                continue; // probablemente un sample u otro archivo pequeño
            }

            // Si cambió de tamaño desde el último sondeo, le damos otra
            // oportunidad (sale de la lista de "sin acción").
            let cambio = tamano_previo.get(&path).map(|p| *p != size).unwrap_or(true);
            if cambio {
                sin_accion.remove(&path);
            }

            let estable_ahora = tamano_previo.get(&path) == Some(&size)
                && edad_archivo(&meta) >= env.estable;
            tamano_previo.insert(path.clone(), size);

            if !estable_ahora || sin_accion.contains(&path) {
                continue;
            }
            // En backoff por un error de red anterior: aún no toca reintentar.
            if let Some((_, hasta)) = reintentos.get(&path) {
                if SystemTime::now() < *hasta {
                    continue;
                }
            }

            log("INFO", format!("Procesando: {}", nombre));
            let padre = path.parent().map(|p| p.to_path_buf());
            let dry = env.opts.dry_run;

            match procesar_archivo(client, &env.opts, cache, &path) {
                Resultado::Renombrado {
                    destino,
                    score,
                    desde_cache,
                } => {
                    let verbo = if dry { "[SIMULADO] movería a" } else { "movido a" };
                    if desde_cache {
                        log("INFO", format!("  -> [caché] {}: {}", verbo, destino));
                    } else if score >= env.opts.umbral {
                        log("INFO", format!("  -> {}: {} (score {:.2})", verbo, destino, score));
                    } else {
                        log(
                            "WARN",
                            format!("  -> FORZADO (score {:.2}) {}: {}", score, verbo, destino),
                        );
                    }
                    reintentos.remove(&path);
                    if dry {
                        // El archivo sigue ahí: no lo reprocesamos cada sondeo.
                        sin_accion.insert(path.clone());
                    } else {
                        tamano_previo.remove(&path);
                        if env.limpiar_vacios {
                            if let Some(p) = padre {
                                borrar_dir_vacio_bajo(&p, &env.watch_dir);
                            }
                        }
                    }
                }
                Resultado::EnviadoARevisar { destino, motivo } => {
                    let verbo = if dry {
                        "[SIMULADO] se enviaría a revisar"
                    } else {
                        "enviado a revisar"
                    };
                    log("WARN", format!("  -> {} · {}: {}", motivo, verbo, destino));
                    reintentos.remove(&path);
                    if dry {
                        sin_accion.insert(path.clone());
                    } else {
                        tamano_previo.remove(&path);
                        if env.limpiar_vacios {
                            if let Some(p) = padre {
                                borrar_dir_vacio_bajo(&p, &env.watch_dir);
                            }
                        }
                    }
                }
                Resultado::DejadoEnSitio { motivo } => {
                    log("WARN", format!("  -> {} · se deja en su sitio", motivo));
                    reintentos.remove(&path);
                    sin_accion.insert(path.clone());
                }
                Resultado::NoEsSerieConEpisodio => {
                    log(
                        "WARN",
                        "  -> modo serie pero el nombre no tiene patrón de episodio; se ignora",
                    );
                    reintentos.remove(&path);
                    sin_accion.insert(path.clone());
                }
                Resultado::ErrorRed(e) => {
                    // Transitorio: backoff exponencial (2·intervalo, 4·, 8·…)
                    // con techo de 15 min. El archivo no se toca.
                    let intentos = reintentos.get(&path).map(|(n, _)| *n).unwrap_or(0) + 1;
                    let factor = 2u32.saturating_pow(intentos.min(5));
                    let espera = (env.intervalo * factor).min(Duration::from_secs(900));
                    log(
                        "WARN",
                        format!(
                            "  -> {} · reintento {} en ~{}s",
                            e,
                            intentos,
                            espera.as_secs()
                        ),
                    );
                    reintentos.insert(path.clone(), (intentos, SystemTime::now() + espera));
                }
                Resultado::Error(e) => {
                    log("ERROR", format!("  -> {}", e));
                    reintentos.remove(&path);
                    sin_accion.insert(path.clone());
                }
            }
        }

        // Evitar que los mapas crezcan sin límite: olvidamos lo que ya no existe.
        tamano_previo.retain(|p, _| p.exists());
        sin_accion.retain(|p| p.exists());
        reintentos.retain(|p, _| p.exists());

        thread::sleep(env.intervalo);
    }
}
