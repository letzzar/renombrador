#!/usr/bin/env bash
# Muestra los logs del servicio en vivo (con marcas de tiempo).
set -euo pipefail
cd "$(dirname "$0")/.."
exec docker compose logs -f -t renombrador
