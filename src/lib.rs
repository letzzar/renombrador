//! Lógica compartida del renombrador.
//!
//! Todo lo "puro" (parseo de nombres, llamadas a TMDb, movimiento seguro de
//! archivos, caché de series y orquestación) vive aquí para que lo reutilicen
//! tanto el **servicio** (`src/main.rs`) como la **GUI** original
//! (`src/bin/renombrador-gui.rs`, feature `gui`).

pub mod cache;
pub mod config;
pub mod mover;
pub mod organizer;
pub mod parse;
pub mod tmdb;

/// Extensiones de vídeo que el renombrador reconoce.
pub const EXTENSIONES_VALIDAS: [&str; 3] = ["mkv", "mp4", "avi"];

/// Umbral de similitud Jaro-Winkler [0,1] por encima del cual se considera
/// "match seguro" y se renombra sin intervención humana.
pub const UMBRAL_AUTO: f64 = 0.85;
