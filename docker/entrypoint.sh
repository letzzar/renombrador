#!/usr/bin/env bash
# Punto de entrada del contenedor. Valida lo mínimo y arranca el servicio.
set -euo pipefail

if [ -z "${TMDB_API_KEY:-}" ]; then
  echo "[ERROR] Falta TMDB_API_KEY." >&2
  echo "[ERROR] Defínela en el archivo .env o en docker-compose.yml antes de arrancar." >&2
  exit 1
fi

echo "[INFO] Lanzando Renombrador TMDb (servicio)..."

# exec para que el proceso del daemon sea PID 1 y reciba SIGTERM al parar.
exec "$@"
