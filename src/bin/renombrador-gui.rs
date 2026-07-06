#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! App de escritorio original (egui). La lógica pura (parseo, TMDb, movimiento
//! de archivos) vive en la librería `renombrador` y se reutiliza aquí.
//! Se compila solo con la feature `gui`: `cargo run --features gui --bin renombrador-gui`.

use arboard::Clipboard;
use eframe::egui;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        mpsc::{self, Receiver, Sender},
        Arc, Mutex,
    },
    thread,
};

use renombrador::config::Idioma;
use renombrador::mover::renombrar_si_seguro;
use renombrador::parse::{
    clave_cache_titulo, extraer_info_archivo, limpiar_nombre_archivo, EpisodioInfo,
};
use renombrador::tmdb::{
    buscar_candidatos_pelicula, buscar_candidatos_serie, buscar_nombre_episodio,
    buscar_nombre_serie, Candidato,
};
use renombrador::{EXTENSIONES_VALIDAS, UMBRAL_AUTO};

const ARCHIVO_CONFIG: &str = "config.json";
const NOMBRE_APP: &str = "renombrador";

// Resuelve la ruta del config en la carpeta estándar del usuario:
//   Windows: %APPDATA%\renombrador\config.json
//   macOS:   ~/Library/Application Support/renombrador/config.json
//   Linux:   ~/.config/renombrador/config.json
fn ruta_config() -> PathBuf {
    match dirs::config_dir() {
        Some(base) => base.join(NOMBRE_APP).join(ARCHIVO_CONFIG),
        None => PathBuf::from(ARCHIVO_CONFIG),
    }
}

// Estructura para guardar la configuración. `cache_series` mapea un título
// normalizado (extraído del nombre del archivo) al `series_id` de TMDb que el
// usuario eligió en una sesión anterior, para saltar la búsqueda interactiva
// en futuras ejecuciones con la misma serie.
#[derive(Serialize, Deserialize, Default)]
struct Config {
    api_key: String,
    directorio: String,
    #[serde(default)]
    cache_series: HashMap<String, i64>,
}

// Archivo que el motor no pudo resolver con confianza alta y queda esperando
// a que el usuario elija un candidato en la UI.
struct Pendiente {
    id: u64,
    ruta_original: PathBuf,
    titulo_extraido: String,
    /// Año de estreno extraído del nombre de archivo, si lo traía.
    anio_extraido: Option<u32>,
    episodio_info: Option<EpisodioInfo>,
    es_serie: bool,
    extension: String,
    candidatos: Vec<Candidato>,
    busqueda_manual: String,
    buscando_manual: bool,
    recordar: bool,
}

// Mensajes desde el hilo de procesamiento hacia la UI.
enum MensajeUI {
    Log(String),
    NuevoPendiente(Pendiente),
    Fin,
}

struct AppRenombrador {
    api_key: String,
    directorio: String,
    logs: String,
    procesando: bool,
    msg_rx: Receiver<MensajeUI>,
    msg_tx: Sender<MensajeUI>,
    modo_series: bool,
    formato_episodio: bool,
    idioma_titulo: Idioma,
    pendientes: Arc<Mutex<Vec<Pendiente>>>,
    siguiente_id: Arc<Mutex<u64>>,
    cache_series: Arc<Mutex<HashMap<String, i64>>>,
}

impl Default for AppRenombrador {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        let mut app = Self {
            api_key: String::new(),
            directorio: String::new(),
            logs: String::new(),
            procesando: false,
            msg_rx: rx,
            msg_tx: tx,
            modo_series: false,
            formato_episodio: true,
            idioma_titulo: Idioma::EsES,
            pendientes: Arc::new(Mutex::new(Vec::new())),
            siguiente_id: Arc::new(Mutex::new(1)),
            cache_series: Arc::new(Mutex::new(HashMap::new())),
        };
        app.cargar_config();
        app
    }
}

impl AppRenombrador {
    fn cargar_config(&mut self) {
        let ruta = ruta_config();

        // Migración suave: si no existe en la carpeta estándar pero sí en el
        // directorio de trabajo (instalaciones antiguas), trasladarlo.
        let legacy = PathBuf::from(ARCHIVO_CONFIG);
        if !ruta.exists() && legacy.exists() {
            if let Some(padre) = ruta.parent() {
                let _ = fs::create_dir_all(padre);
            }
            if fs::rename(&legacy, &ruta).is_err() {
                if fs::copy(&legacy, &ruta).is_ok() {
                    let _ = fs::remove_file(&legacy);
                }
            }
        }

        if let Ok(contenido) = fs::read_to_string(&ruta) {
            if let Ok(config) = serde_json::from_str::<Config>(&contenido) {
                self.api_key = config.api_key;
                self.directorio = config.directorio;
                if let Ok(mut cache) = self.cache_series.lock() {
                    *cache = config.cache_series;
                }
            }
        }
    }

    fn guardar_config(&self) {
        let ruta = ruta_config();
        if let Some(padre) = ruta.parent() {
            if let Err(e) = fs::create_dir_all(padre) {
                let _ = self.msg_tx.send(MensajeUI::Log(format!(
                    "Aviso: no se pudo crear la carpeta de configuración ({}): {}",
                    padre.display(),
                    e
                )));
                return;
            }
        }
        let cache = self
            .cache_series
            .lock()
            .map(|c| c.clone())
            .unwrap_or_default();
        let config = Config {
            api_key: self.api_key.clone(),
            directorio: self.directorio.clone(),
            cache_series: cache,
        };
        if let Ok(json) = serde_json::to_string_pretty(&config) {
            let _ = fs::write(&ruta, json);
        }
    }

    fn iniciar_proceso(&mut self, ctx: egui::Context) {
        self.procesando = true;
        self.logs.clear();
        if let Ok(mut p) = self.pendientes.lock() {
            p.clear();
        }
        self.guardar_config();

        let api_key = self.api_key.clone();
        let directorio = self.directorio.clone();
        let tx = self.msg_tx.clone();
        let modo_series = self.modo_series;
        let formato_episodio = self.formato_episodio;
        let idioma_titulo = self.idioma_titulo;
        let cache_arc = self.cache_series.clone();
        let siguiente_id = self.siguiente_id.clone();

        thread::spawn(move || {
            let _ = tx.send(MensajeUI::Log(format!(
                "Iniciando análisis en: {}\n{}",
                directorio,
                "-".repeat(40)
            )));
            ctx.request_repaint();

            let archivos = match fs::read_dir(&directorio) {
                Ok(it) => it,
                Err(_) => {
                    let _ = tx.send(MensajeUI::Log(
                        "Error: No se pudo leer el directorio.".to_string(),
                    ));
                    let _ = tx.send(MensajeUI::Fin);
                    return;
                }
            };

            let client = Client::new();

            for entrada in archivos.flatten() {
                let path = entrada.path();
                if !path.is_file() {
                    continue;
                }

                let ext = match path.extension().and_then(|e| e.to_str()) {
                    Some(e) if EXTENSIONES_VALIDAS.contains(&e.to_lowercase().as_str()) => {
                        e.to_lowercase()
                    }
                    _ => continue,
                };

                let nombre_archivo = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                let _ = tx.send(MensajeUI::Log(format!("Procesando: {}", nombre_archivo)));
                ctx.request_repaint();

                let (titulo_extraido, anio_extraido, episodio_info) =
                    extraer_info_archivo(&nombre_archivo);
                let tratar_como_serie = modo_series || episodio_info.is_some();

                if tratar_como_serie {
                    let ep = match episodio_info {
                        Some(e) => e,
                        None => {
                            let _ = tx.send(MensajeUI::Log(
                                "  -> No se detectó episodio en el nombre del archivo."
                                    .to_string(),
                            ));
                            continue;
                        }
                    };

                    // Capa 4: si la serie está cacheada, saltar búsqueda.
                    let clave = clave_cache_titulo(&titulo_extraido, anio_extraido);
                    let id_cacheado = cache_arc
                        .lock()
                        .ok()
                        .and_then(|c| c.get(&clave).copied());

                    if let Some(series_id) = id_cacheado {
                        match renombrar_episodio_con_id(
                            &client,
                            &api_key,
                            idioma_titulo,
                            series_id,
                            ep,
                            &path,
                            &ext,
                            formato_episodio,
                            &directorio,
                        ) {
                            Ok(nuevo) => {
                                let _ = tx.send(MensajeUI::Log(format!(
                                    "  -> [caché] Renombrado a: {}",
                                    nuevo
                                )));
                            }
                            Err(e) => {
                                let _ = tx.send(MensajeUI::Log(format!(
                                    "  -> [caché] Error: {}",
                                    e
                                )));
                            }
                        }
                        ctx.request_repaint();
                        continue;
                    }

                    // Capa 2: búsqueda progresiva multi-query.
                    let candidatos = match buscar_candidatos_serie(
                        &client,
                        &titulo_extraido,
                        anio_extraido,
                        &api_key,
                        idioma_titulo,
                    ) {
                        Ok(c) => c,
                        Err(e) => {
                            let _ = tx.send(MensajeUI::Log(format!(
                                "  -> Error de red: {}. Se omite; vuelve a procesar más tarde.",
                                e
                            )));
                            ctx.request_repaint();
                            continue;
                        }
                    };

                    if candidatos.is_empty() {
                        let _ = tx.send(MensajeUI::Log(format!(
                            "  -> Sin resultados en TMDb para '{}'. Pendiente de revisión.",
                            titulo_extraido
                        )));
                        let id = {
                            let mut g = siguiente_id.lock().unwrap();
                            let id = *g;
                            *g += 1;
                            id
                        };
                        let _ = tx.send(MensajeUI::NuevoPendiente(Pendiente {
                            id,
                            ruta_original: path.clone(),
                            titulo_extraido: titulo_extraido.clone(),
                            anio_extraido,
                            episodio_info: Some(ep),
                            es_serie: true,
                            extension: ext.clone(),
                            candidatos: Vec::new(),
                            busqueda_manual: titulo_extraido.clone(),
                            buscando_manual: false,
                            recordar: false,
                        }));
                        ctx.request_repaint();
                        continue;
                    }

                    let mejor = &candidatos[0];
                    // La variante desesperada nunca auto-renombra por score.
                    if mejor.score >= UMBRAL_AUTO && mejor.fiable {
                        match renombrar_episodio_con_id(
                            &client,
                            &api_key,
                            idioma_titulo,
                            mejor.id,
                            ep,
                            &path,
                            &ext,
                            formato_episodio,
                            &directorio,
                        ) {
                            Ok(nuevo) => {
                                let _ = tx.send(MensajeUI::Log(format!(
                                    "  -> Renombrado a: {} (score {:.2})",
                                    nuevo, mejor.score
                                )));
                            }
                            Err(e) => {
                                let _ = tx
                                    .send(MensajeUI::Log(format!("  -> Error al renombrar: {}", e)));
                            }
                        }
                    } else {
                        let _ = tx.send(MensajeUI::Log(format!(
                            "  -> Match dudoso (mejor score {:.2}). Pendiente de revisión.",
                            mejor.score
                        )));
                        let id = {
                            let mut g = siguiente_id.lock().unwrap();
                            let id = *g;
                            *g += 1;
                            id
                        };
                        let _ = tx.send(MensajeUI::NuevoPendiente(Pendiente {
                            id,
                            ruta_original: path.clone(),
                            titulo_extraido: titulo_extraido.clone(),
                            anio_extraido,
                            episodio_info: Some(ep),
                            es_serie: true,
                            extension: ext.clone(),
                            candidatos,
                            busqueda_manual: titulo_extraido.clone(),
                            buscando_manual: false,
                            recordar: false,
                        }));
                    }
                    ctx.request_repaint();
                } else {
                    // Película
                    let candidatos = match buscar_candidatos_pelicula(
                        &client,
                        &titulo_extraido,
                        anio_extraido,
                        &api_key,
                        idioma_titulo,
                    ) {
                        Ok(c) => c,
                        Err(e) => {
                            let _ = tx.send(MensajeUI::Log(format!(
                                "  -> Error de red: {}. Se omite; vuelve a procesar más tarde.",
                                e
                            )));
                            ctx.request_repaint();
                            continue;
                        }
                    };

                    if candidatos.is_empty() {
                        let _ = tx.send(MensajeUI::Log(format!(
                            "  -> Sin resultados en TMDb para '{}'. Pendiente de revisión.",
                            titulo_extraido
                        )));
                        let id = {
                            let mut g = siguiente_id.lock().unwrap();
                            let id = *g;
                            *g += 1;
                            id
                        };
                        let _ = tx.send(MensajeUI::NuevoPendiente(Pendiente {
                            id,
                            ruta_original: path.clone(),
                            titulo_extraido: titulo_extraido.clone(),
                            anio_extraido,
                            episodio_info: None,
                            es_serie: false,
                            extension: ext.clone(),
                            candidatos: Vec::new(),
                            busqueda_manual: titulo_extraido.clone(),
                            buscando_manual: false,
                            recordar: false,
                        }));
                        ctx.request_repaint();
                        continue;
                    }

                    let mejor = &candidatos[0];
                    // La variante desesperada nunca auto-renombra por score.
                    if mejor.score >= UMBRAL_AUTO && mejor.fiable {
                        let titulo_limpio = limpiar_nombre_archivo(&mejor.titulo);
                        let nuevo_nombre =
                            format!("{} [{}].{}", titulo_limpio, mejor.anio, ext);
                        let mut nueva_ruta = PathBuf::from(&directorio);
                        nueva_ruta.push(&nuevo_nombre);
                        if path.file_name().map(|n| n.to_string_lossy()).as_deref()
                            == Some(nuevo_nombre.as_str())
                        {
                            let _ = tx.send(MensajeUI::Log(
                                "  -> Ya tiene el formato correcto. Omitiendo.".to_string(),
                            ));
                        } else {
                            match renombrar_si_seguro(&path, &nueva_ruta) {
                                Ok(_) => {
                                    let _ = tx.send(MensajeUI::Log(format!(
                                        "  -> Renombrado a: {} (score {:.2})",
                                        nuevo_nombre, mejor.score
                                    )));
                                }
                                Err(e) => {
                                    let _ = tx.send(MensajeUI::Log(format!(
                                        "  -> Error al renombrar: {}",
                                        e
                                    )));
                                }
                            }
                        }
                    } else {
                        let _ = tx.send(MensajeUI::Log(format!(
                            "  -> Match dudoso (mejor score {:.2}). Pendiente de revisión.",
                            mejor.score
                        )));
                        let id = {
                            let mut g = siguiente_id.lock().unwrap();
                            let id = *g;
                            *g += 1;
                            id
                        };
                        let _ = tx.send(MensajeUI::NuevoPendiente(Pendiente {
                            id,
                            ruta_original: path.clone(),
                            titulo_extraido: titulo_extraido.clone(),
                            anio_extraido,
                            episodio_info: None,
                            es_serie: false,
                            extension: ext.clone(),
                            candidatos,
                            busqueda_manual: titulo_extraido.clone(),
                            buscando_manual: false,
                            recordar: false,
                        }));
                    }
                    ctx.request_repaint();
                }
            }

            let _ = tx.send(MensajeUI::Log(format!(
                "{}\n¡Proceso de búsqueda finalizado!",
                "-".repeat(40)
            )));
            let _ = tx.send(MensajeUI::Fin);
            ctx.request_repaint();
        });
    }

    // Aplica un candidato concreto a un pendiente: renombra el archivo y
    // —opcionalmente— recuerda la elección y la propaga a otros pendientes
    // del mismo lote con el mismo título extraído.
    fn aplicar_candidato(
        &mut self,
        pendiente_id: u64,
        candidato_idx: usize,
        propagar_a_lote: bool,
        ctx: &egui::Context,
    ) {
        let (pendiente, candidato) = {
            let pendientes = self.pendientes.lock().unwrap();
            let pos = match pendientes.iter().position(|p| p.id == pendiente_id) {
                Some(p) => p,
                None => return,
            };
            let p = &pendientes[pos];
            if candidato_idx >= p.candidatos.len() {
                return;
            }
            (
                ClonPendiente::from(p),
                p.candidatos[candidato_idx].clone(),
            )
        };

        let directorio = self.directorio.clone();
        let api_key = self.api_key.clone();
        let idioma = self.idioma_titulo;
        let formato_ep = self.formato_episodio;
        let recordar = pendiente.recordar;
        let tx = self.msg_tx.clone();
        let pendientes_arc = self.pendientes.clone();
        let cache_arc = self.cache_series.clone();
        let ctx2 = ctx.clone();

        // Resolver en un hilo para no bloquear la UI mientras se llama a TMDb.
        thread::spawn(move || {
            let client = Client::new();
            let resultado = aplicar_un_pendiente(
                &client,
                &api_key,
                idioma,
                formato_ep,
                &directorio,
                &pendiente,
                &candidato,
            );
            match resultado {
                Ok(nuevo) => {
                    let _ = tx.send(MensajeUI::Log(format!(
                        "  -> [manual] Renombrado a: {}",
                        nuevo
                    )));
                }
                Err(e) => {
                    let _ = tx.send(MensajeUI::Log(format!(
                        "  -> [manual] Error: {}",
                        e
                    )));
                }
            }

            // Caché (capa 4)
            if recordar && pendiente.es_serie && candidato.media_type == "tv" {
                if let Ok(mut cache) = cache_arc.lock() {
                    cache.insert(
                        clave_cache_titulo(&pendiente.titulo_extraido, pendiente.anio_extraido),
                        candidato.id,
                    );
                }
            }

            // Quitar el pendiente resuelto y, si toca, propagar al lote.
            let claves_a_propagar = {
                let mut lista = pendientes_arc.lock().unwrap();
                if let Some(idx) = lista.iter().position(|p| p.id == pendiente.id) {
                    lista.remove(idx);
                }
                if propagar_a_lote && pendiente.es_serie && candidato.media_type == "tv" {
                    let clave =
                        clave_cache_titulo(&pendiente.titulo_extraido, pendiente.anio_extraido);
                    lista
                        .iter()
                        .filter(|p| {
                            p.es_serie
                                && clave_cache_titulo(&p.titulo_extraido, p.anio_extraido) == clave
                        })
                        .map(ClonPendiente::from)
                        .collect::<Vec<_>>()
                } else {
                    Vec::new()
                }
            };

            for hermano in &claves_a_propagar {
                let res = aplicar_un_pendiente(
                    &client,
                    &api_key,
                    idioma,
                    formato_ep,
                    &directorio,
                    hermano,
                    &candidato,
                );
                match res {
                    Ok(nuevo) => {
                        let _ = tx.send(MensajeUI::Log(format!(
                            "  -> [lote] Renombrado a: {}",
                            nuevo
                        )));
                    }
                    Err(e) => {
                        let _ = tx.send(MensajeUI::Log(format!(
                            "  -> [lote] Error: {}",
                            e
                        )));
                    }
                }
                if let Ok(mut lista) = pendientes_arc.lock() {
                    if let Some(idx) = lista.iter().position(|p| p.id == hermano.id) {
                        lista.remove(idx);
                    }
                }
            }

            ctx2.request_repaint();
        });
    }

    fn lanzar_busqueda_manual(&self, pendiente_id: u64, ctx: &egui::Context) {
        let (query, es_serie) = {
            let mut pendientes = self.pendientes.lock().unwrap();
            let pos = match pendientes.iter().position(|p| p.id == pendiente_id) {
                Some(p) => p,
                None => return,
            };
            let p = &mut pendientes[pos];
            if p.busqueda_manual.trim().is_empty() {
                return;
            }
            p.buscando_manual = true;
            (p.busqueda_manual.clone(), p.es_serie)
        };

        let api_key = self.api_key.clone();
        let idioma = self.idioma_titulo;
        let pendientes_arc = self.pendientes.clone();
        let ctx2 = ctx.clone();

        thread::spawn(move || {
            let client = Client::new();
            // En la búsqueda manual un error de red se muestra como 0
            // resultados; el usuario puede pulsar "Buscar" de nuevo.
            // Query libre del usuario: sin filtro de año.
            let candidatos = if es_serie {
                buscar_candidatos_serie(&client, &query, None, &api_key, idioma)
            } else {
                buscar_candidatos_pelicula(&client, &query, None, &api_key, idioma)
            }
            .unwrap_or_default();
            if let Ok(mut lista) = pendientes_arc.lock() {
                if let Some(p) = lista.iter_mut().find(|p| p.id == pendiente_id) {
                    p.candidatos = candidatos;
                    p.buscando_manual = false;
                }
            }
            ctx2.request_repaint();
        });
    }

    fn omitir_pendiente(&self, pendiente_id: u64) {
        if let Ok(mut lista) = self.pendientes.lock() {
            if let Some(idx) = lista.iter().position(|p| p.id == pendiente_id) {
                let p = &lista[idx];
                let _ = self.msg_tx.send(MensajeUI::Log(format!(
                    "  -> Omitido: {}",
                    p.ruta_original
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default()
                )));
                lista.remove(idx);
            }
        }
    }
}

// "Snapshot" plano de un Pendiente para mover entre hilos sin retener el lock.
#[derive(Clone)]
struct ClonPendiente {
    id: u64,
    ruta_original: PathBuf,
    titulo_extraido: String,
    anio_extraido: Option<u32>,
    episodio_info: Option<EpisodioInfo>,
    es_serie: bool,
    extension: String,
    recordar: bool,
}

impl From<&Pendiente> for ClonPendiente {
    fn from(p: &Pendiente) -> Self {
        Self {
            id: p.id,
            ruta_original: p.ruta_original.clone(),
            titulo_extraido: p.titulo_extraido.clone(),
            anio_extraido: p.anio_extraido,
            episodio_info: p.episodio_info,
            es_serie: p.es_serie,
            extension: p.extension.clone(),
            recordar: p.recordar,
        }
    }
}

fn aplicar_un_pendiente(
    client: &Client,
    api_key: &str,
    idioma: Idioma,
    formato_episodio: bool,
    directorio: &str,
    pendiente: &ClonPendiente,
    candidato: &Candidato,
) -> Result<String, String> {
    if pendiente.es_serie && candidato.media_type == "tv" {
        let ep = pendiente
            .episodio_info
            .ok_or_else(|| "sin info de episodio".to_string())?;
        let nombre_episodio =
            buscar_nombre_episodio(client, api_key, idioma, candidato.id, ep.temporada, ep.episodio);
        let titulo_limpio = limpiar_nombre_archivo(&candidato.titulo);
        let formato_ep = if formato_episodio {
            format!("{}x{:02}", ep.temporada, ep.episodio)
        } else {
            format!("S{:02}E{:02}", ep.temporada, ep.episodio)
        };
        let nuevo_nombre = if let Some(ep_name) = nombre_episodio {
            let ep_name_limpio = limpiar_nombre_archivo(&ep_name);
            format!(
                "{} {} {}.{}",
                titulo_limpio, formato_ep, ep_name_limpio, pendiente.extension
            )
        } else {
            format!(
                "{} {}.{}",
                titulo_limpio, formato_ep, pendiente.extension
            )
        };
        let mut nueva_ruta = PathBuf::from(directorio);
        nueva_ruta.push(&nuevo_nombre);
        if pendiente
            .ruta_original
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .as_deref()
            == Some(nuevo_nombre.as_str())
        {
            return Ok(format!("(ya tenía el formato correcto) {}", nuevo_nombre));
        }
        renombrar_si_seguro(&pendiente.ruta_original, &nueva_ruta)?;
        Ok(nuevo_nombre)
    } else if !pendiente.es_serie && candidato.media_type == "movie" {
        let titulo_limpio = limpiar_nombre_archivo(&candidato.titulo);
        let nuevo_nombre = format!("{} [{}].{}", titulo_limpio, candidato.anio, pendiente.extension);
        let mut nueva_ruta = PathBuf::from(directorio);
        nueva_ruta.push(&nuevo_nombre);
        if pendiente
            .ruta_original
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .as_deref()
            == Some(nuevo_nombre.as_str())
        {
            return Ok(format!("(ya tenía el formato correcto) {}", nuevo_nombre));
        }
        renombrar_si_seguro(&pendiente.ruta_original, &nueva_ruta)?;
        Ok(nuevo_nombre)
    } else {
        Err(format!(
            "tipo de candidato '{}' no coincide con el tipo del archivo",
            candidato.media_type
        ))
    }
}

fn renombrar_episodio_con_id(
    client: &Client,
    api_key: &str,
    idioma: Idioma,
    series_id: i64,
    ep: EpisodioInfo,
    path: &Path,
    ext: &str,
    formato_episodio: bool,
    directorio: &str,
) -> Result<String, String> {
    let (titulo_serie, _anio) = buscar_nombre_serie(client, api_key, idioma, series_id)
        .map_err(|e| format!("no se pudo obtener info de la serie {}: {}", series_id, e))?;
    let nombre_episodio =
        buscar_nombre_episodio(client, api_key, idioma, series_id, ep.temporada, ep.episodio);
    let titulo_limpio = limpiar_nombre_archivo(&titulo_serie);
    let formato_ep = if formato_episodio {
        format!("{}x{:02}", ep.temporada, ep.episodio)
    } else {
        format!("S{:02}E{:02}", ep.temporada, ep.episodio)
    };
    let nuevo_nombre = if let Some(ep_name) = nombre_episodio {
        let ep_name_limpio = limpiar_nombre_archivo(&ep_name);
        format!("{} {} {}.{}", titulo_limpio, formato_ep, ep_name_limpio, ext)
    } else {
        format!("{} {}.{}", titulo_limpio, formato_ep, ext)
    };
    let mut nueva_ruta = PathBuf::from(directorio);
    nueva_ruta.push(&nuevo_nombre);
    if path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .as_deref()
        == Some(nuevo_nombre.as_str())
    {
        return Ok(format!("(ya tenía el formato correcto) {}", nuevo_nombre));
    }
    renombrar_si_seguro(path, &nueva_ruta)?;
    Ok(nuevo_nombre)
}

impl eframe::App for AppRenombrador {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Drenar mensajes del hilo de fondo
        while let Ok(msg) = self.msg_rx.try_recv() {
            match msg {
                MensajeUI::Log(s) => {
                    self.logs.push_str(&s);
                    self.logs.push('\n');
                }
                MensajeUI::NuevoPendiente(p) => {
                    if let Ok(mut lista) = self.pendientes.lock() {
                        lista.push(p);
                    }
                }
                MensajeUI::Fin => {
                    self.procesando = false;
                    // Guardar config para persistir cache si se acumuló
                    self.guardar_config();
                }
            }
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Renombrador de Películas y Series (TMDb)");
            ui.add_space(10.0);

            ui.label(egui::RichText::new("API Key de TMDb:").strong());
            ui.add(egui::TextEdit::singleline(&mut self.api_key).desired_width(f32::INFINITY));
            ui.add_space(10.0);

            ui.label(egui::RichText::new("Directorio a procesar:").strong());
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.directorio).desired_width(400.0));
                if ui.button("Examinar...").clicked() {
                    if let Some(carpeta) = rfd::FileDialog::new().pick_folder() {
                        self.directorio = carpeta.display().to_string();
                    }
                }
            });
            ui.add_space(15.0);

            ui.label(egui::RichText::new("Modo:").strong());
            ui.horizontal(|ui| {
                if ui.radio(!self.modo_series, "Películas").clicked() {
                    self.modo_series = false;
                }
                if ui.radio(self.modo_series, "Series").clicked() {
                    self.modo_series = true;
                }
            });
            ui.add_space(15.0);

            if self.modo_series {
                ui.label(egui::RichText::new("Formato de episodios:").strong());
                ui.horizontal(|ui| {
                    if ui.radio(self.formato_episodio, "1x05").clicked() {
                        self.formato_episodio = true;
                    }
                    if ui.radio(!self.formato_episodio, "S01E05").clicked() {
                        self.formato_episodio = false;
                    }
                });
                ui.add_space(15.0);
            }

            ui.label(egui::RichText::new("Idioma del título:").strong());
            ui.horizontal(|ui| {
                egui::ComboBox::from_label("")
                    .selected_text(self.idioma_titulo.nombre())
                    .show_ui(ui, |ui| {
                        for idioma in Idioma::todas() {
                            ui.selectable_value(&mut self.idioma_titulo, idioma, idioma.nombre());
                        }
                    });
            });
            ui.add_space(15.0);

            let btn_text = if self.procesando { "Procesando..." } else { "Iniciar Renombrado" };
            if ui
                .add_enabled(!self.procesando, egui::Button::new(btn_text))
                .clicked()
            {
                if !self.api_key.is_empty() && !self.directorio.is_empty() {
                    self.iniciar_proceso(ctx.clone());
                }
            }
            ui.add_space(10.0);

            // Botón de gestión de caché
            let cache_size = self
                .cache_series
                .lock()
                .map(|c| c.len())
                .unwrap_or(0);
            ui.horizontal(|ui| {
                ui.label(format!("Caché de series recordadas: {}", cache_size));
                if cache_size > 0 && ui.button("Vaciar caché").clicked() {
                    if let Ok(mut c) = self.cache_series.lock() {
                        c.clear();
                    }
                    self.guardar_config();
                }
            });

            ui.add_space(15.0);
            self.dibujar_panel_pendientes(ui, ctx);

            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Registro de actividad:").strong());
                if ui.button("📋 Copiar todo").clicked() {
                    if let Ok(mut cb) = Clipboard::new() {
                        let _ = cb.set_text(self.logs.clone());
                    }
                }
            });

            egui::ScrollArea::vertical()
                .auto_shrink([false; 2])
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    ui.add_sized(
                        ui.available_size(),
                        egui::TextEdit::multiline(&mut self.logs).interactive(true),
                    );
                });
        });
    }
}

impl AppRenombrador {
    fn dibujar_panel_pendientes(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let num_pendientes = self
            .pendientes
            .lock()
            .map(|l| l.len())
            .unwrap_or(0);

        let header = format!("⚠ Pendientes de revisión ({})", num_pendientes);
        egui::CollapsingHeader::new(header)
            .default_open(num_pendientes > 0)
            .show(ui, |ui| {
                if num_pendientes == 0 {
                    ui.label("No hay archivos pendientes de revisión.");
                    return;
                }

                // Acciones que se generan dentro del bucle y se aplican fuera
                // para no mantener el lock más tiempo del necesario.
                let mut accion_aplicar: Option<(u64, usize, bool)> = None;
                let mut accion_buscar: Option<u64> = None;
                let mut accion_omitir: Option<u64> = None;

                {
                    let mut lista = match self.pendientes.lock() {
                        Ok(l) => l,
                        Err(_) => return,
                    };

                    egui::ScrollArea::vertical()
                        .max_height(300.0)
                        .auto_shrink([false, false])
                        .id_source("scroll_pendientes")
                        .show(ui, |ui| {
                            for p in lista.iter_mut() {
                                ui.group(|ui| {
                                    let nombre = p
                                        .ruta_original
                                        .file_name()
                                        .map(|n| n.to_string_lossy().to_string())
                                        .unwrap_or_default();
                                    ui.label(
                                        egui::RichText::new(&nombre)
                                            .strong()
                                            .monospace(),
                                    );
                                    ui.horizontal(|ui| {
                                        ui.label("Título extraído:");
                                        ui.monospace(&p.titulo_extraido);
                                        if let Some(ep) = p.episodio_info {
                                            ui.label(format!(
                                                "  ·  S{:02}E{:02}",
                                                ep.temporada, ep.episodio
                                            ));
                                        }
                                    });

                                    if p.candidatos.is_empty() {
                                        ui.label(
                                            egui::RichText::new(
                                                "Sin candidatos. Prueba una búsqueda manual.",
                                            )
                                            .italics(),
                                        );
                                    } else {
                                        ui.add_space(4.0);
                                        ui.label(
                                            egui::RichText::new("Candidatos:").strong(),
                                        );
                                        for (idx, c) in p.candidatos.iter().enumerate() {
                                            ui.horizontal(|ui| {
                                                if ui
                                                    .button(format!(
                                                        "Usar #{}", idx + 1
                                                    ))
                                                    .clicked()
                                                {
                                                    accion_aplicar =
                                                        Some((p.id, idx, false));
                                                }
                                                if p.es_serie
                                                    && c.media_type == "tv"
                                                    && ui
                                                        .button("Usar + aplicar a lote")
                                                        .clicked()
                                                {
                                                    accion_aplicar =
                                                        Some((p.id, idx, true));
                                                }
                                                ui.label(format!(
                                                    "[{:.2}] {} ({})  ·  orig: {}  ·  pop: {:.0}",
                                                    c.score,
                                                    c.titulo,
                                                    c.anio,
                                                    c.nombre_original,
                                                    c.popularidad
                                                ));
                                            });
                                            if !c.overview.is_empty() {
                                                ui.label(
                                                    egui::RichText::new(&c.overview)
                                                        .small()
                                                        .weak(),
                                                );
                                            }
                                        }
                                    }

                                    ui.add_space(6.0);
                                    ui.horizontal(|ui| {
                                        ui.label("Búsqueda manual:");
                                        ui.add(
                                            egui::TextEdit::singleline(
                                                &mut p.busqueda_manual,
                                            )
                                            .desired_width(250.0),
                                        );
                                        let btn = if p.buscando_manual {
                                            "Buscando..."
                                        } else {
                                            "Buscar"
                                        };
                                        if ui
                                            .add_enabled(
                                                !p.buscando_manual,
                                                egui::Button::new(btn),
                                            )
                                            .clicked()
                                        {
                                            accion_buscar = Some(p.id);
                                        }
                                    });

                                    ui.horizontal(|ui| {
                                        if p.es_serie {
                                            ui.checkbox(
                                                &mut p.recordar,
                                                "Recordar serie para futuras ejecuciones",
                                            );
                                        }
                                        if ui.button("Omitir").clicked() {
                                            accion_omitir = Some(p.id);
                                        }
                                    });
                                });
                                ui.add_space(4.0);
                            }
                        });
                }

                if let Some((id, idx, propagar)) = accion_aplicar {
                    self.aplicar_candidato(id, idx, propagar, ctx);
                }
                if let Some(id) = accion_buscar {
                    self.lanzar_busqueda_manual(id, ctx);
                }
                if let Some(id) = accion_omitir {
                    self.omitir_pendiente(id);
                }
            });
    }
}

// Decodifica el .ico embebido a píxeles RGBA para el icono de la ventana.
fn cargar_icono_ventana() -> Option<egui::IconData> {
    const ICONO_BYTES: &[u8] = include_bytes!("../../logo_app.ico");
    let imagen =
        image::load_from_memory_with_format(ICONO_BYTES, image::ImageFormat::Ico).ok()?;
    let rgba = imagen.to_rgba8();
    let (width, height) = rgba.dimensions();
    Some(egui::IconData {
        rgba: rgba.into_raw(),
        width,
        height,
    })
}

fn main() -> eframe::Result<()> {
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([720.0, 720.0])
        .with_title("Renombrador TMDb");
    if let Some(icono) = cargar_icono_ventana() {
        viewport = viewport.with_icon(icono);
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "Renombrador de Peliculas",
        options,
        Box::new(|_cc| Box::<AppRenombrador>::default()),
    )
}
