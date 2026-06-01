//! Caché persistente de series: mapea un título normalizado al `series_id` de
//! TMDb ya resuelto, para no repetir la búsqueda con cada episodio.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

#[derive(Default)]
pub struct SeriesCache {
    map: HashMap<String, i64>,
    path: PathBuf,
}

impl SeriesCache {
    /// Carga la caché desde `path` (si no existe o está corrupta, empieza vacía).
    pub fn cargar(path: PathBuf) -> Self {
        let map = fs::read_to_string(&path)
            .ok()
            .and_then(|c| serde_json::from_str::<HashMap<String, i64>>(&c).ok())
            .unwrap_or_default();
        SeriesCache { map, path }
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
