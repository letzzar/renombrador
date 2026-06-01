//! Idioma de los títulos pedidos a TMDb.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Idioma {
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
    /// Código BCP-47 que entiende la API de TMDb (parámetro `language`).
    pub fn codigo(&self) -> &'static str {
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

    /// Nombre legible (usado por la GUI).
    pub fn nombre(&self) -> &'static str {
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

    pub fn todas() -> Vec<Idioma> {
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

    /// Resuelve un código de idioma desde texto (variable de entorno
    /// `TMDB_LANGUAGE`). Acepta tanto `es-ES` como abreviaturas (`es`, `en`...).
    pub fn from_codigo(s: &str) -> Option<Idioma> {
        match s.trim().to_lowercase().replace('_', "-").as_str() {
            "es-es" => Some(Idioma::EsES),
            "es-mx" | "es-419" | "es-la" => Some(Idioma::EsMX),
            "en-us" => Some(Idioma::EnUS),
            "en-gb" => Some(Idioma::EnGB),
            "fr-fr" | "fr" => Some(Idioma::FrFR),
            "de-de" | "de" => Some(Idioma::DeDE),
            "it-it" | "it" => Some(Idioma::ItIT),
            "pt-br" | "pt" => Some(Idioma::PtBR),
            "es" => Some(Idioma::EsES),
            "en" => Some(Idioma::EnUS),
            _ => None,
        }
    }
}
