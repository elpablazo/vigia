# Vigía

[![CI](https://github.com/elpablazo/vigia/actions/workflows/ci.yml/badge.svg)](https://github.com/elpablazo/vigia/actions/workflows/ci.yml)
[![MIT](https://img.shields.io/badge/licencia-MIT-blue.svg)](LICENSE)

Vigila páginas web y avisa cuando cambian: precio, existencias o texto. Un solo binario (Rust), SQLite, interfaz incluida, API REST y servidor MCP.

*Self-hosted web page change monitor (price, stock, text) in a single static Rust binary, with a REST API, webhooks and an MCP server. Docs are in Spanish.*

## Correr

```bash
docker run -d --name vigia -p 8080:8080 -v vigia-data:/data \
  -e VIGIA_TOKEN="$(openssl rand -hex 24)" ghcr.io/elpablazo/vigia
```

O descarga el binario estático de [Releases](https://github.com/elpablazo/vigia/releases) (Linux x86-64, trae su suma SHA-256), o compila: `VIGIA_TOKEN=… cargo run --release`.

| Variable | Para qué | Por defecto |
|---|---|---|
| `VIGIA_TOKEN` | Token de acceso (interfaz y API). Si falta, se genera uno por ejecución y se imprime en el log. | — |
| `VIGIA_DB` | Ruta del archivo SQLite | `/data/vigia.db` en Docker |
| `PORT` | Puerto | `8080` |
| `VIGIA_ALLOW_PRIVATE` | `1` permite vigilar direcciones de red privada | apagado |

## Cómo decide qué vigilar

Pega una URL. Si la página publica datos de producto (JSON-LD de schema.org, metaetiquetas `product:price` o microdatos), vigila **precio y existencias**. Si no, vigila el **texto visible** (de toda la página o del selector CSS que indiques) y guarda las diferencias línea por línea.

## API

Autenticación: `Authorization: Bearer $VIGIA_TOKEN`. Todo es JSON; los errores llegan como `{"error": "…"}`. Tiempos en segundos unix, precios como número.

| Método | Ruta | Qué hace |
|---|---|---|
| `GET` | `/api/watches` | Lista todo, con `spark` (historial de precio) y `changes` |
| `POST` | `/api/watches` | `{url, tag?, interval?, selector?, mode?}`. Hace la primera revisión antes de responder |
| `GET` | `/api/watches/{id}` | Una vigilancia con `history` |
| `PATCH` | `/api/watches/{id}` | `{tag?, interval?, selector?, mode?, paused?}` |
| `DELETE` | `/api/watches/{id}` | Elimina la vigilancia y su historial |
| `POST` | `/api/watches/{id}/check` | Revisa ahora → `{changed, watch}` |
| `GET` | `/api/watches/{id}/snapshots/{snap}/diff` | Diferencias de texto contra la versión anterior |
| `POST` | `/api/extract` | `{url, selector?}`. Lee una página una vez, sin guardarla |
| `GET` `PUT` | `/api/settings` | `webhook_url`, `default_interval`, `user_agent` |
| `GET` | `/api/status` | Versión, memoria, agentes conectados |

```bash
curl -X POST https://tu-vigia/api/watches -H "Authorization: Bearer $VIGIA_TOKEN" \
  -H "content-type: application/json" -d '{"url":"https://tienda.example/producto"}'
```

### Webhook

Con `webhook_url` configurado, cada cambio se envía como `POST`:

```json
{ "event": "change", "watch": { "id": "…", "url": "…", "title": "…", "price": 27639, "currency": "MXN", "in_stock": true },
  "before": { "price": 28999, "in_stock": true }, "after": { "price": 27639, "in_stock": true }, "diff": null }
```

En vigilancias de texto, `diff` trae las diferencias en formato unificado.

### MCP

`POST /mcp` (Streamable HTTP) con el mismo token. Herramientas: `list_watches`, `add_watch`, `get_watch`, `check_watch`, `remove_watch`, `extract`.

```bash
claude mcp add --transport http vigia https://tu-vigia/mcp --header "Authorization: Bearer $VIGIA_TOKEN"
```

## Agentes

Algunos sitios rechazan las IP de centros de datos. Un agente es este mismo binario corriendo en otra red (tu casa, tu laptop): se conecta al servidor hacia afuera, sin abrir puertos, y pide las páginas desde ahí.

```bash
VIGIA_SERVER=https://tu-vigia VIGIA_TOKEN=… VIGIA_AGENT_NAME=casa vigia agent

# o con Docker
docker run -d --restart unless-stopped --name vigia-agent -e VIGIA_SERVER=https://tu-vigia \
  -e VIGIA_TOKEN=… -e VIGIA_AGENT_NAME=casa ghcr.io/elpablazo/vigia agent
```

El servidor intenta primero directo. Si el sitio lo rechaza (HTTP 403/429 o un desafío anti-bots) y hay un agente conectado, repite la petición por el agente y recuerda esa ruta para esa vigilancia.

## Lo que no hace

- No ejecuta JavaScript: las páginas que arman su contenido en el navegador llegan vacías y se reportan así.
- No resuelve CAPTCHAs ni desafíos anti-bots, y no disfraza su identidad: se presenta como `Vigia/<versión>` (el User-Agent se puede cambiar en Ajustes). Un sitio que desafía al servidor se marca como «Bloqueado».
- Un solo usuario (un token). Avisos sólo por webhook.

## Desarrollo

```bash
cargo test
VIGIA_TOKEN=dev cargo run   # http://localhost:8080
```

Código: `src/main.rs` (API, revisiones, programador), `src/extract.rs` (HTML → precio/texto), `src/fetch.rs` (peticiones y agentes), `src/mcp.rs`, `src/ui.html` (interfaz, sin dependencias).

## Publicación y despliegue

Cada commit en `main` pasa por GitHub Actions: pruebas, imagen en `ghcr.io/elpablazo/vigia` y una rama `deploy` cuyo único archivo es un `Dockerfile` que apunta a esa imagen por su digest. Un servidor que siga la rama `deploy` (Coolify con despliegue automático, por ejemplo) se actualiza solo y sin compilar nada. Los pull requests sólo corren las pruebas; se despliegan al fusionarse.

Para una versión con binario descargable, sube la versión en `Cargo.toml`, etiqueta y empuja:

```bash
git tag v0.1.0 && git push origin v0.1.0
```

El Release lleva el binario estático, su suma SHA-256 y su atestación de procedencia (`gh attestation verify vigia-x86_64-linux --repo elpablazo/vigia`).

## Seguridad y contribuciones

Reportes de vulnerabilidades: [SECURITY.md](SECURITY.md). Cómo contribuir: [CONTRIBUTING.md](CONTRIBUTING.md).

## Licencia

[MIT](LICENSE)
