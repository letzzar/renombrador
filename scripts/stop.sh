#!/usr/bin/env bash
# Detiene y elimina el contenedor del servicio.
set -euo pipefail
cd "$(dirname "$0")/.."
docker compose down
echo "Servicio detenido."
