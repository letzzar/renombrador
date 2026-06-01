#!/usr/bin/env bash
# Arranca el servicio en segundo plano (construyendo la imagen si hace falta).
set -euo pipefail
cd "$(dirname "$0")/.."

if [ ! -f .env ]; then
  echo "No existe .env; lo creo a partir de .env.example."
  cp .env.example .env
  echo ">> Edita .env y pon tu TMDB_API_KEY y las rutas antes de continuar."
  exit 1
fi

docker compose up -d --build
echo "Servicio arrancado. Logs:  ./scripts/logs.sh"
