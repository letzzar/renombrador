#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use arboard::Clipboard;
use eframe::egui;
use regex::Regex;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
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
use strsim::jaro_winkler;

const BASE_URL: &str = "https://api.themoviedb.org/3";
const ARCHIVO_CONFIG: &str = "config.json";
const NOMBRE_APP: &str = "renombrador";
const EXTENSIONES_VALIDAS: [&str; 3] = ["mkv", "mp4", "avi"];

// Umbral de similitud Jaro-Winkler [0,1] por encima del cual se considera
// "match seguro" y se renombra sin preguntar al usuario.
const UMBRAL_AUTO: f64 = 0.85;

// Cuántos candidatos como máximo se muestran al usuario en el panel de revisión.
const MAX_CANDIDATOS: usize = 5;

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

#[derive(Clone, Copy, Debug)]
struct EpisodioInfo {
    temporada: u32,
    episodio: u32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Idioma {
    EsES,
    EsMX,
    EnUS,
    EnGB,
    FrFR,
    DeDE,
    ItIT,
    PtBR,
}

impl Idioma {
    fn codigo(&self) -> &'static str {
        match self {
            Idioma::EsES => "es-ES",
            Idioma::EsMX => "es-MX",
            Idioma::EnUS => "en-US",
            Idioma::EnGB => "en-GB",
            Idioma::FrFR => "fr-FR",
            Idioma::DeDE => "de-DE",
            Idioma::ItIT => "it-IT",
            Idioma::PtBR => "pt-BR",
        }
    }

    fn nombre(&self) -> &'static str {
        match self {
            Idioma::EsES => "Español (España)",
            Idioma::EsMX => "Español (Latino)",
            Idioma::EnUS => "Inglés (EEUU)",
            Idioma::EnGB => "Inglés (Reino Unido)",
            Idioma::FrFR => "Francés",
            Idioma::DeDE => "Alemán",
            Idioma::ItIT => "Italiano",
            Idioma::PtBR => "Portugués (Brasil)",
        }
    }

    fn todas() -> Vec<Idioma> {
        vec![
            Idioma::EsES,
            Idioma::EsMX,
            Idioma::EnUS,
            Idioma::EnGB,
            Idioma::FrFR,
            Idioma::DeDE,
            Idioma::ItIT,
            Idioma::PtBR,
        ]
    }
}

// Resultado bruto de TMDb para mostrar al usuario en el panel de revisión.
#[derive(Clone, Debug)]
struct Candidato {
    id: i64,
    media_type: String, // "tv" o "movie"
    titulo: String,
    nombre_original: String,
    anio: String,
    popularidad: f64,
    overview: String,
    score: f64, // similitud calculada contra la query original
}

// Archivo que el motor no pudo resolver con confianza alta y queda esperando
// a que el usuario elija un candidato en la UI.
struct Pendiente {
    id: u64,
    ruta_original: PathBuf,
    titulo_extraido: String,
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

                let (titulo_extraido, episodio_info) = extraer_info_archivo(&nombre_archivo);
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
                    let clave = clave_cache(&titulo_extraido);
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
                    let candidatos =
                        buscar_candidatos_serie(&client, &titulo_extraido, &api_key, idioma_titulo);

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
                    if mejor.score >= UMBRAL_AUTO {
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
                    let candidatos = buscar_candidatos_pelicula(
                        &client,
                        &titulo_extraido,
                        &api_key,
                        idioma_titulo,
                    );

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
                    if mejor.score >= UMBRAL_AUTO {
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
                    cache.insert(clave_cache(&pendiente.titulo_extraido), candidato.id);
                }
            }

            // Quitar el pendiente resuelto y, si toca, propagar al lote.
            let claves_a_propagar = {
                let mut lista = pendientes_arc.lock().unwrap();
                if let Some(idx) = lista.iter().position(|p| p.id == pendiente.id) {
                    lista.remove(idx);
                }
                if propagar_a_lote && pendiente.es_serie && candidato.media_type == "tv" {
                    let clave = clave_cache(&pendiente.titulo_extraido);
                    lista
                        .iter()
                        .filter(|p| p.es_serie && clave_cache(&p.titulo_extraido) == clave)
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
            let candidatos = if es_serie {
                buscar_candidatos_serie(&client, &query, &api_key, idioma)
            } else {
                buscar_candidatos_pelicula(&client, &query, &api_key, idioma)
            };
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
        .ok_or_else(|| format!("no se pudo obtener info de la serie {}", series_id))?;
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

// === LÓGICA DE EXTRACCIÓN Y LIMPIEZA ===

fn extraer_info_archivo(nombre_archivo: &str) -> (String, Option<EpisodioInfo>) {
    let path = Path::new(nombre_archivo);
    let nombre_sin_ext = path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    let re_episodio_simple = Regex::new(r"\b(\d+)x(\d+)\b").unwrap();
    let re_episodio_formato = Regex::new(r"[Ss](\d+)[Ee](\d+)").unwrap();

    let (episodio_info, pos_episodio) =
        if let Some(cap) = re_episodio_simple.captures(&nombre_sin_ext) {
            let ep = Some(EpisodioInfo {
                temporada: cap.get(1).and_then(|m| m.as_str().parse().ok()).unwrap_or(1),
                episodio: cap.get(2).and_then(|m| m.as_str().parse().ok()).unwrap_or(1),
            });
            (ep, cap.get(0).map(|m| m.start()))
        } else if let Some(cap) = re_episodio_formato.captures(&nombre_sin_ext) {
            let ep = Some(EpisodioInfo {
                temporada: cap.get(1).and_then(|m| m.as_str().parse().ok()).unwrap_or(1),
                episodio: cap.get(2).and_then(|m| m.as_str().parse().ok()).unwrap_or(1),
            });
            (ep, cap.get(0).map(|m| m.start()))
        } else {
            (None, None)
        };

    let titulo_antes_episodio = if let Some(pos) = pos_episodio {
        nombre_sin_ext[..pos].trim()
    } else {
        &nombre_sin_ext
    };

    let re_año = Regex::new(r"\(([12]\d{3})\)").unwrap();
    let titulo_sin_metadata = if let Some(cap) = re_año.captures(titulo_antes_episodio) {
        &titulo_antes_episodio[..cap.get(0).unwrap().start()]
    } else {
        titulo_antes_episodio
    };

    // Capa 1: limpieza no destructiva. Sustituimos solo separadores típicos
    // de nombres de release (puntos, guiones bajos, guiones) por espacios y
    // colapsamos. Conservamos diacríticos y apóstrofes para no romper títulos
    // como "La maldición de Widow's Bay".
    let re_sep = Regex::new(r"[\._\-]+").unwrap();
    let titulo_espaciado = re_sep.replace_all(titulo_sin_metadata, " ");

    // Quitar caracteres prohibidos en nombres de archivo (y barras varias).
    let re_invalidos = Regex::new(r#"[<>:"/\\|?*]"#).unwrap();
    let limpio = re_invalidos.replace_all(&titulo_espaciado, " ");

    // Colapsar espacios.
    let re_espacios = Regex::new(r"\s+").unwrap();
    let limpio = re_espacios.replace_all(&limpio, " ").trim().to_string();

    (limpio, episodio_info)
}

// Quita un artículo líder ("la", "el", "the", "le"...) si existe.
fn quitar_articulo_lider(s: &str) -> Option<String> {
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

// Devuelve las palabras del título excluyendo stopwords cortas (de, la, del, etc).
fn palabras_fuertes(s: &str) -> Vec<String> {
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

// Genera variantes de búsqueda en orden de prioridad. Cada variante es un par
// (query, idioma) para ir probando contra TMDb hasta encontrar algo útil.
fn variantes_busqueda(titulo: &str, idioma: Idioma) -> Vec<(String, Idioma)> {
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

// Calcula similitud Jaro-Winkler comparando la query contra el mejor de
// (titulo, nombre_original). Usamos formas en minúsculas para que diacríticos
// y mayúsculas no penalicen.
fn similitud(query: &str, titulo: &str, nombre_original: &str) -> f64 {
    let q = query.to_lowercase();
    let a = jaro_winkler(&q, &titulo.to_lowercase());
    let b = jaro_winkler(&q, &nombre_original.to_lowercase());
    a.max(b)
}

// Clave de caché: título normalizado (lower + colapso de espacios) que se usa
// como key del HashMap `cache_series` y para agrupar pendientes del mismo lote.
fn clave_cache(titulo: &str) -> String {
    titulo
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

// === LLAMADAS A TMDb ===

fn buscar_candidatos_serie(
    client: &Client,
    titulo: &str,
    api_key: &str,
    idioma: Idioma,
) -> Vec<Candidato> {
    let url = format!("{}/search/tv", BASE_URL);
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
                        if ids_vistos.contains(&id) {
                            continue;
                        }
                        ids_vistos.insert(id);
                        let titulo = item
                            .get("name")
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_string();
                        let original = item
                            .get("original_name")
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_string();
                        let fecha = item
                            .get("first_air_date")
                            .and_then(|d| d.as_str())
                            .unwrap_or("");
                        let anio = if fecha.len() >= 4 {
                            fecha[..4].to_string()
                        } else {
                            "0000".to_string()
                        };
                        let popularidad =
                            item.get("popularity").and_then(|p| p.as_f64()).unwrap_or(0.0);
                        let overview: String = item
                            .get("overview")
                            .and_then(|o| o.as_str())
                            .unwrap_or("")
                            .chars()
                            .take(180)
                            .collect();
                        // Score se rellena más abajo contra el título extraído original
                        // para que el ranking sea comparable entre variantes de query.
                        todos.push(Candidato {
                            id,
                            media_type: "tv".to_string(),
                            titulo,
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

    // Calcular score contra el título extraído original y ordenar.
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

fn buscar_candidatos_pelicula(
    client: &Client,
    titulo: &str,
    api_key: &str,
    idioma: Idioma,
) -> Vec<Candidato> {
    let url = format!("{}/search/movie", BASE_URL);
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
                        if ids_vistos.contains(&id) {
                            continue;
                        }
                        ids_vistos.insert(id);
                        let titulo_c = item
                            .get("title")
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_string();
                        let original = item
                            .get("original_title")
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_string();
                        let fecha = item
                            .get("release_date")
                            .and_then(|d| d.as_str())
                            .unwrap_or("");
                        let anio = if fecha.len() >= 4 {
                            fecha[..4].to_string()
                        } else {
                            "0000".to_string()
                        };
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
                            media_type: "movie".to_string(),
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

fn buscar_nombre_serie(
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
    let fecha = datos
        .get("first_air_date")
        .and_then(|d| d.as_str())
        .unwrap_or("");
    let anio = if fecha.len() >= 4 {
        fecha[..4].to_string()
    } else {
        "0000".to_string()
    };
    Some((nombre, anio))
}

fn buscar_nombre_episodio(
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

// === UTILIDADES DE NOMBRE DE ARCHIVO ===

// Renombra `origen` a `destino` sin sobreescribir un archivo distinto preexistente.
fn renombrar_si_seguro(origen: &Path, destino: &Path) -> Result<(), String> {
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

fn limpiar_nombre_archivo(nombre: &str) -> String {
    let re_invalidos = Regex::new(r#"[<>:"/\\|?*]"#).unwrap();
    let limpio = re_invalidos.replace_all(nombre, "");

    let re_espacios = Regex::new(" +").unwrap();
    let limpio = re_espacios.replace_all(&limpio, " ");

    limpio.trim().to_string()
}

// Decodifica el .ico embebido a píxeles RGBA para el icono de la ventana.
fn cargar_icono_ventana() -> Option<egui::IconData> {
    const ICONO_BYTES: &[u8] = include_bytes!("../logo_app.ico");
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
