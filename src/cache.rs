//! Caché persistente de series: mapea un título normalizado al `series_id` de
//! TMDb ya resuelto, para no repetir la búsqueda con cada episodio.
//!
//! Además guarda dos cachés **solo en memoria** (no se persisten porque los
//! datos pueden cambiar en TMDb entre ejecuciones):
//! - info básica de cada serie (nombre y año) por id, y
//! - los nombres de episodio de cada temporada ya consultada.
//!
//! Con ambas, procesar una temporada completa de N capítulos cuesta ~2
//! llamadas a TMDb en lugar de ~2·N.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

#[derive(Default)]
pub struct SeriesCache {
    map: HashMap<String, i64>,
    path: PathBuf,
    /// (id de serie) -> (nombre, año). Solo en memoria.
    series_info: HashMap<i64, (String, String)>,
    /// (id de serie, temporada) -> (nº episodio -> nombre). Solo en memoria.
    temporadas: HashMap<(i64, u32), HashMap<u32, String>>,
}

impl SeriesCache {
    /// Carga la caché desde `path` (si no existe o está corrupta, empieza vacía).
    pub fn cargar(path: PathBuf) -> Self {
        let map = fs::read_to_string(&path)
            .ok()
            .and_then(|c| serde_json::from_str::<HashMap<String, i64>>(&c).ok())
            .unwrap_or_default();
        SeriesCache {
            map,
            path,
            series_info: HashMap::new(),
            temporadas: HashMap::new(),
        }
    }

    pub fn get(&self, clave: &str) -> Option<i64> {
        self.map.get(clave).copied()
    }

    /// Inserta una entrada y persiste inmediatamente en disco.
    pub fn insertar(&mut self, clave: String, id: i64) {
        if self.map.get(&clave) == Some(&id) {
            return;
        }
        self.map.insert(clave, id);
        self.guardar();
    }

    /// Elimina una entrada (p. ej. un id que ya no existe en TMDb) y persiste.
    pub fn eliminar(&mut self, clave: &str) {
        if self.map.remove(clave).is_some() {
            self.guardar();
        }
    }

    /// Info básica (nombre, año) de una serie ya consultada en esta ejecución.
    pub fn serie_info(&self, id: i64) -> Option<&(String, String)> {
        self.series_info.get(&id)
    }

    pub fn insertar_serie_info(&mut self, id: i64, nombre: String, anio: String) {
        self.series_info.insert(id, (nombre, anio));
    }

    /// Nombres de episodio de una temporada ya consultada en esta ejecución.
    pub fn temporada(&self, serie: i64, temporada: u32) -> Option<&HashMap<u32, String>> {
        self.temporadas.get(&(serie, temporada))
    }

    pub fn insertar_temporada(
        &mut self,
        serie: i64,
        temporada: u32,
        episodios: HashMap<u32, String>,
    ) {
        self.temporadas.insert((serie, temporada), episodios);
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    fn guardar(&self) {
        if let Some(padre) = self.path.parent() {
            let _ = fs::create_dir_all(padre);
        }
        if let Ok(json) = serde_json::to_string_pretty(&self.map) {
            let _ = fs::write(&self.path, json);
        }
    }
}
