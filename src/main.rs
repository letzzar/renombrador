#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use regex::Regex;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender},
    thread,
};
use clipboard_win::{formats, set_clipboard};

const BASE_URL: &str = "https://api.themoviedb.org/3";
const ARCHIVO_CONFIG: &str = "config.json";
const EXTENSIONES_VALIDAS: [&str; 3] = ["mkv", "mp4", "avi"];

// Estructura para guardar la configuración
#[derive(Serialize, Deserialize, Default)]
struct Config {
    api_key: String,
    directorio: String,
}

// Estructura para información de episodios
#[derive(Clone)]
struct EpisodioInfo {
    temporada: u32,
    episodio: u32,
}

// Estructura para el resultado de búsqueda
#[derive(Clone)]
struct ResultadoCatalogo {
    titulo: String,
    anio: String,
    episodio: Option<EpisodioInfo>,
    nombre_episodio: Option<String>,
}

// Idiomas disponibles
#[derive(Clone, Copy, Debug, PartialEq)]
enum Idioma {
    EsES,    // Español de España
    EsMX,    // Español Latino
    EnUS,    // Inglés (EEUU)
    EnGB,    // Inglés (Reino Unido)
    FrFR,    // Francés
    DeDE,    // Alemán
    ItIT,    // Italiano
    PtBR,    // Portugués Brasil
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

// Estructura principal de la App
struct AppRenombrador {
    api_key: String,
    directorio: String,
    logs: String,
    procesando: bool,
    log_rx: Receiver<String>, // Recibe mensajes del hilo secundario
    log_tx: Sender<String>,   // Envía mensajes desde el hilo secundario
    modo_series: bool,        // true = series, false = películas
    formato_episodio: bool,   // true = 1x05, false = S01E05
    idioma_titulo: Idioma,    // Idioma para los títulos
}

impl Default for AppRenombrador {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        let mut app = Self {
            api_key: String::new(),
            directorio: String::new(),
            logs: String::new(),
            procesando: false,
            log_rx: rx,
            log_tx: tx,
            modo_series: false,      // Por defecto: películas
            formato_episodio: true,  // Por defecto: 1x05
            idioma_titulo: Idioma::EsES, // Por defecto: Español de España
        };
        app.cargar_config();
        app
    }
}

impl AppRenombrador {
    fn cargar_config(&mut self) {
        if let Ok(contenido) = fs::read_to_string(ARCHIVO_CONFIG) {
            if let Ok(config) = serde_json::from_str::<Config>(&contenido) {
                self.api_key = config.api_key;
                self.directorio = config.directorio;
            }
        }
    }

    fn guardar_config(&self) {
        let config = Config {
            api_key: self.api_key.clone(),
            directorio: self.directorio.clone(),
        };
        if let Ok(json) = serde_json::to_string_pretty(&config) {
            let _ = fs::write(ARCHIVO_CONFIG, json);
        }
    }

    fn iniciar_proceso(&mut self, ctx: egui::Context) {
        self.procesando = true;
        self.logs.clear();
        self.guardar_config();

        let api_key = self.api_key.clone();
        let directorio = self.directorio.clone();
        let tx = self.log_tx.clone();
        let modo_series = self.modo_series;
        let formato_episodio = self.formato_episodio;
        let idioma_titulo = self.idioma_titulo;

        // Iniciamos un hilo separado (equivalente a threading.Thread en Python)
        thread::spawn(move || {
            let _ = tx.send(format!("Iniciando análisis en: {}\n{}", directorio, "-".repeat(40)));
            ctx.request_repaint(); // Le decimos a la UI que se actualice

            if let Ok(archivos) = fs::read_dir(&directorio) {
                let client = Client::new();

                for entrada in archivos.flatten() {
                    let path = entrada.path();
                    if !path.is_file() { continue; }

                    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                        if EXTENSIONES_VALIDAS.contains(&ext.to_lowercase().as_str()) {
                            let nombre_archivo = path.file_name().unwrap().to_string_lossy().to_string();
                            let _ = tx.send(format!("Procesando: {}", nombre_archivo));
                            ctx.request_repaint();

                            if modo_series {
                                // Lógica para series
                                let (titulo_busqueda, episodio_info) = extraer_info_archivo(&nombre_archivo);
                                if let Some(ep) = episodio_info {
                                    let resultado = buscar_serie_en_tmdb(&client, &titulo_busqueda, &api_key, ep.temporada, ep.episodio, idioma_titulo);
                                    
                                    if let Some(info) = resultado {
                                        let formato_ep = if formato_episodio {
                                            format!("{}x{:02}", info.episodio.as_ref().unwrap().temporada, info.episodio.as_ref().unwrap().episodio)
                                        } else {
                                            format!("S{:02}E{:02}", info.episodio.as_ref().unwrap().temporada, info.episodio.as_ref().unwrap().episodio)
                                        };
                                        
                                        // Limpiar nombre de serie y episodio de caracteres inválidos
                                        let titulo_limpio = limpiar_nombre_archivo(&info.titulo);
                                        let nuevo_nombre = if let Some(ep_name) = &info.nombre_episodio {
                                            let ep_name_limpio = limpiar_nombre_archivo(ep_name);
                                            format!("{} {} {}.{}", titulo_limpio, formato_ep, ep_name_limpio, ext.to_lowercase())
                                        } else {
                                            format!("{} {}.{}", titulo_limpio, formato_ep, ext.to_lowercase())
                                        };
                                        let mut nueva_ruta = PathBuf::from(&directorio);
                                        nueva_ruta.push(&nuevo_nombre);

                                        if path.file_name().unwrap() == nuevo_nombre.as_str() {
                                            let _ = tx.send("  -> Ya tiene el formato correcto. Omitiendo.".to_string());
                                        } else {
                                            match fs::rename(&path, &nueva_ruta) {
                                                Ok(_) => { let _ = tx.send(format!("  -> Renombrado a: {}", nuevo_nombre)); },
                                                Err(e) => { let _ = tx.send(format!("  -> Error al renombrar: {}", e)); },
                                            }
                                        }
                                    } else {
                                        let _ = tx.send(format!("  -> No encontrado en TMDb: '{}'", titulo_busqueda));
                                    }
                                } else {
                                    let _ = tx.send("  -> No se detectó episodio en el nombre del archivo.".to_string());
                                }
                            } else {
                                // Lógica para películas (original)
                                let (titulo_busqueda, episodio_info) = extraer_info_archivo(&nombre_archivo);
                                let resultado = buscar_en_tmdb(&client, &titulo_busqueda, &api_key, episodio_info.clone(), idioma_titulo);

                                if let Some(info) = resultado {
                                    let titulo_limpio = limpiar_nombre_archivo(&info.titulo);
                                    let nuevo_nombre = format!("{} [{}].{}", titulo_limpio, info.anio, ext.to_lowercase());
                                    let mut nueva_ruta = PathBuf::from(&directorio);
                                    nueva_ruta.push(&nuevo_nombre);

                                    if path.file_name().unwrap() == nuevo_nombre.as_str() {
                                        let _ = tx.send("  -> Ya tiene el formato correcto. Omitiendo.".to_string());
                                    } else {
                                        match fs::rename(&path, &nueva_ruta) {
                                            Ok(_) => { let _ = tx.send(format!("  -> Renombrado a: {}", nuevo_nombre)); },
                                            Err(e) => { let _ = tx.send(format!("  -> Error al renombrar: {}", e)); },
                                        }
                                    }
                                } else {
                                    let _ = tx.send(format!("  -> No encontrado en TMDb: '{}'", titulo_busqueda));
                                }
                            }
                            ctx.request_repaint();
                        }
                    }
                }
            } else {
                let _ = tx.send("Error: No se pudo leer el directorio.".to_string());
            }

            let _ = tx.send(format!("{}\n¡Proceso finalizado!", "-".repeat(40)));
            let _ = tx.send("FIN_PROCESO".to_string()); // Señal especial
            ctx.request_repaint();
        });
    }
}

// Implementación de la interfaz gráfica (equivalente a los widgets de Tkinter)
impl eframe::App for AppRenombrador {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Escuchar mensajes del hilo en segundo plano
        while let Ok(msg) = self.log_rx.try_recv() {
            if msg == "FIN_PROCESO" {
                self.procesando = false;
            } else {
                self.logs.push_str(&msg);
                self.logs.push('\n');
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
                    // Equivalente a filedialog.askdirectory()
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
            if ui.add_enabled(!self.procesando, egui::Button::new(btn_text)).clicked() {
                if !self.api_key.is_empty() && !self.directorio.is_empty() {
                    self.iniciar_proceso(ctx.clone());
                }
            }

            ui.add_space(15.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Registro de actividad:").strong());
                if ui.button("📋 Copiar todo").clicked() {
                    let _ = set_clipboard(formats::Unicode, &self.logs);
                }
            });
            
            // Equivalente a scrolledtext.ScrolledText
            egui::ScrollArea::vertical()
                .auto_shrink([false; 2])
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    ui.add_sized(
                        ui.available_size(),
                        egui::TextEdit::multiline(&mut self.logs).interactive(true)
                    );
                });
        });
    }
}

// === LÓGICA CORE (Traducción exacta de tu Python) ===

fn extraer_info_archivo(nombre_archivo: &str) -> (String, Option<EpisodioInfo>) {
    let path = Path::new(nombre_archivo);
    let nombre_sin_ext = path.file_stem().unwrap_or_default().to_string_lossy().to_string();
    
    let re_episodio_simple = Regex::new(r"\b(\d+)x(\d+)\b").unwrap();
    let re_episodio_formato = Regex::new(r"[Ss](\d+)[Ee](\d+)").unwrap();
    
    let (episodio_info, pos_episodio) = if let Some(cap) = re_episodio_simple.captures(&nombre_sin_ext) {
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
    
    let re_sep = Regex::new(r"[\._-]").unwrap();
    let titulo_espaciado = re_sep.replace_all(titulo_sin_metadata, " ");
    
    let palabras: Vec<&str> = titulo_espaciado
        .split_whitespace()
        .filter(|w| w.chars().any(|c| c.is_alphabetic()))
        .collect();
    
    let titulo_limpio = palabras.join(" ");
    
    let re_invalidos = Regex::new(r#"[<>:\"/|?*]"#).unwrap();
    let titulo_final = re_invalidos.replace_all(&titulo_limpio, "").to_string();

    (titulo_final.trim().to_string(), episodio_info)
}

fn buscar_en_tmdb(client: &Client, query: &str, api_key: &str, episodio: Option<EpisodioInfo>, idioma: Idioma) -> Option<ResultadoCatalogo> {
    let url = format!("{}/search/multi", BASE_URL);
    
    let resp = client.get(&url)
        .query(&[("api_key", api_key), ("query", query), ("language", idioma.codigo())])
        .send();

    if let Ok(respuesta) = resp {
        if let Ok(datos) = respuesta.json::<Value>() {
            if let Some(resultados) = datos.get("results").and_then(|r| r.as_array()) {
                if let Some(resultado) = resultados.first() {
                    let tipo = resultado.get("media_type").and_then(|t| t.as_str()).unwrap_or("movie");
                    
                    let mut titulo_oficial = None;
                    let mut fecha = "";

                    if tipo == "movie" {
                        titulo_oficial = resultado.get("title").and_then(|t| t.as_str());
                        fecha = resultado.get("release_date").and_then(|d| d.as_str()).unwrap_or("");
                    } else if tipo == "tv" {
                        titulo_oficial = resultado.get("name").and_then(|t| t.as_str());
                        fecha = resultado.get("first_air_date").and_then(|d| d.as_str()).unwrap_or("");
                    }

                    let anio = if fecha.len() >= 4 { &fecha[..4] } else { "0000" }.to_string();
                    
                    if let Some(tit) = titulo_oficial {
                        let titulo_seguro = limpiar_nombre_archivo(tit);
                        
                        let episodio_info = if tipo == "tv" && episodio.is_some() {
                            episodio
                        } else {
                            None
                        };
                        
                        return Some(ResultadoCatalogo {
                            titulo: titulo_seguro,
                            anio,
                            episodio: episodio_info,
                            nombre_episodio: None,
                        });
                    }
                }
            }
        }
    }
    None
}

fn limpiar_titulo_serie(titulo: &str) -> String {
    let limpio = titulo.to_lowercase();
    
    let re_invalidos = Regex::new(r"[^a-z0-9 ]").unwrap();
    let limpio = re_invalidos.replace_all(&limpio, " ");
    
    let re_espacios = Regex::new(r" +").unwrap();
    let limpio = re_espacios.replace_all(&limpio, " ");
    
    limpio.trim().to_string()
}

fn limpiar_nombre_archivo(nombre: &str) -> String {
    let re_invalidos = Regex::new(r#"[<>:\"/|?*]"#).unwrap();
    let limpio = re_invalidos.replace_all(nombre, "");
    
    let re_espacios = Regex::new(" +").unwrap();
    let limpio = re_espacios.replace_all(&limpio, " ");
    
    limpio.trim().to_string()
}

fn buscar_serie_en_tmdb(client: &Client, titulo: &str, api_key: &str, temporada: u32, episodio: u32, idioma: Idioma) -> Option<ResultadoCatalogo> {
    let titulo_limpio = limpiar_titulo_serie(titulo);
    
    let titulo_busqueda = if titulo_limpio.is_empty() { titulo } else { &titulo_limpio };
    
    let url_busqueda = format!("{}/search/tv", BASE_URL);
    
    let resp_busqueda = client.get(&url_busqueda)
        .query(&[("api_key", api_key), ("query", titulo_busqueda), ("language", idioma.codigo())])
        .send();

    if let Ok(respuesta) = resp_busqueda {
        if let Ok(datos) = respuesta.json::<Value>() {
            if let Some(resultados) = datos.get("results").and_then(|r| r.as_array()) {
                if let Some(resultado) = resultados.first() {
                    let titulo_oficial = resultado.get("name").and_then(|t| t.as_str());
                    let fecha = resultado.get("first_air_date").and_then(|d| d.as_str()).unwrap_or("");
                    let series_id = resultado.get("id").and_then(|id| id.as_i64());
                    
                    let anio = if fecha.len() >= 4 { &fecha[..4] } else { "0000" }.to_string();
                    
                    if let (Some(tit), Some(id)) = (titulo_oficial, series_id) {
                        let titulo_seguro = limpiar_nombre_archivo(tit);
                        
                        // Paso 2: Obtener información del episodio (solo con api_key)
                        let ep_url = format!("{}/tv/{}/season/{}/episode/{}", BASE_URL, id, temporada, episodio);
                        if let Ok(ep_resp) = client.get(&ep_url)
                            .query(&[("api_key", api_key), ("language", idioma.codigo())])
                            .send() {
                            if let Ok(ep_datos) = ep_resp.json::<Value>() {
                                let nombre_episodio = ep_datos.get("name").and_then(|n| n.as_str()).map(|s| s.to_string());
                                
                                return Some(ResultadoCatalogo {
                                    titulo: titulo_seguro,
                                    anio,
                                    episodio: Some(EpisodioInfo {
                                        temporada,
                                        episodio,
                                    }),
                                    nombre_episodio,
                                });
                            }
                        }
                        
                        // Si falla la búsqueda del episodio, devolver solo la serie
                        return Some(ResultadoCatalogo {
                            titulo: titulo_seguro,
                            anio,
                            episodio: Some(EpisodioInfo {
                                temporada,
                                episodio,
                            }),
                            nombre_episodio: None,
                        });
                    }
                }
            }
        }
    }
    None
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([600.0, 500.0])
            .with_title("Renombrador TMDb"),
        ..Default::default()
    };
    eframe::run_native(
        "Renombrador de Peliculas",
        options,
        Box::new(|_cc| Box::<AppRenombrador>::default()),
    )
}