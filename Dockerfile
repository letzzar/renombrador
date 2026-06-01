# syntax=docker/dockerfile:1

# ============================================================================
#  Etapa 1: compilación del servicio (solo el binario daemon, sin la GUI).
# ============================================================================
# Último estable de Rust 1.x: necesario porque algunas dependencias recientes
# (rustls, icu/url…) exigen una versión de Rust nueva.
FROM rust:1-slim-bookworm AS builder

# Sin dependencias extra: usamos TLS rustls (puro Rust), no hace falta OpenSSL.
# La imagen rust:slim ya incluye gcc/libc-dev para enlazar.
WORKDIR /app

# Copiamos manifiestos y fuentes. La feature `gui` queda desactivada, así que
# eframe y compañía NO se compilan: el binario es pequeño y el build, rápido.
COPY Cargo.toml Cargo.lock ./
COPY build.rs ./build.rs
COPY src ./src

RUN cargo build --release --bin renombrador-daemon

# ============================================================================
#  Etapa 2: imagen final mínima.
# ============================================================================
FROM debian:bookworm-slim

# Certificados raíz y zona horaria (TLS lo resuelve rustls; sin libssl).
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates tzdata \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/renombrador-daemon /usr/local/bin/renombrador-daemon
COPY docker/entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh

# Rutas por defecto dentro del contenedor (se montan como volúmenes).
ENV WATCH_DIR=/descargas \
    MOVIES_DIR=/peliculas \
    SERIES_DIR=/series \
    CACHE_FILE=/config/cache.json \
    TMDB_LANGUAGE=es-ES

VOLUME ["/descargas", "/peliculas", "/series", "/config"]

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
CMD ["renombrador-daemon"]
