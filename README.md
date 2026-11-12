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

Pega una o varias URLs. Si la página publica datos de producto (JSON-LD de schema.org, aunque venga mal formado, metaetiquetas `product:price` o microdatos), vigila **precio y existencias**. Si le das un selector CSS que apunte al precio, ese precio manda. Si no hay precio, vigila el **texto visible** (de toda la página o del selector), con las líneas que quieras ignorar, y guarda las diferencias línea por línea. Las respuestas JSON se comparan formateadas.

## Instalar en el teléfono y recibir avisos

La interfaz es una app instalable (PWA). En Android/Chrome: menú → Instalar app, o el botón «Instalar». En iPhone (iOS 16.4+): Compartir → Agregar a inicio y abre Vigía desde ese ícono. Luego toca la campana para recibir avisos push en ese dispositivo; los avisos van cifrados de punta a punta (RFC 8291) y no pasan por ningún servicio de terceros además del servicio push del propio navegador.

Cada vigilancia elige cuándo avisar: con cualquier cambio, sólo si baja, si llega a un precio objetivo, si vuelve a haber existencias o nunca. Si una vigilancia falla tres veces seguidas, también avisa.

## Historial

Los precios se guardan sólo cuando cambian y nunca se borran: unos 60 bytes por cambio. Cien productos que cambien de precio todos los días durante cinco años ocupan unos 10 MB. El texto de las páginas se guarda comprimido (últimas 50 versiones). La gráfica de cada producto muestra los ejes de precio y fecha, los periodos agotados y el precio exacto en cualquier punto; las estadísticas (mínimo, máximo, promedio ponderado por tiempo, cambios) salen para 7 días, 1 mes, 3 meses, 1 año y todo.

Para análisis: `GET /api/export/prices.csv` trae cada cambio de precio de todas las vigilancias, y `GET /api/backup` descarga la base completa (SQLite).

## API

Autenticación: `Authorization: Bearer $VIGIA_TOKEN`. Todo es JSON; los errores llegan como `{"error": "…"}`. Tiempos en segundos unix, precios como número.

| Método | Ruta | Qué hace |
|---|---|---|
| `GET` | `/api/watches` | Lista todo, con `spark` (historial de precio) y `changes` |
| `POST` | `/api/watches` | `{url, tag?, interval?, selector?, mode?, notify?, target?}`. Hace la primera revisión antes de responder |
| `GET` | `/api/watches/{id}` | Una vigilancia con `prices` (serie completa), `stats` por periodo e `history` |
| `PATCH` | `/api/watches/{id}` | `{tag?, interval?, selector?, mode?, paused?, notify?, target?, ignore?}` |
| `DELETE` | `/api/watches/{id}` | Elimina la vigilancia y su historial |
| `POST` | `/api/watches/{id}/check` | Revisa ahora → `{changed, watch}` |
| `GET` | `/api/watches/{id}/snapshots/{snap}/diff` | Diferencias de texto contra la versión anterior |
| `POST` | `/api/extract` | `{url, selector?}`. Lee una página una vez, sin guardarla |
| `GET` `PUT` | `/api/settings` | `webhook_url`, `default_interval`, `user_agent`, `relay_url` |
| `GET` | `/api/status` | Versión, memoria, tamaño de la base, dispositivos con avisos |
| `GET` | `/api/export/prices.csv` · `/api/backup` | Exportar precios · respaldo completo |
| `POST` | `/api/push/subscribe` · `/api/push/test` | Registrar un dispositivo para avisos · enviar uno de prueba |

`notify`: `all` (por defecto), `drop`, `below` (con `target`), `restock` o `none`. `ignore`: líneas a ignorar en páginas de texto, una por línea.

```bash
curl -X POST https://tu-vigia/api/watches -H "Authorization: Bearer $VIGIA_TOKEN" \
  -H "content-type: application/json" -d '{"url":"https://tienda.example/producto"}'
```

### Webhook

Con `webhook_url` configurado, cada cambio se envía como `POST`:

```json
{ "event": "change", "message": "Bajó a $27,639 MXN (antes $28,999, -4.7 %)",
  "watch": { "id": "…", "url": "…", "title": "…", "price": 27639, "currency": "MXN", "in_stock": true },
  "before": { "price": 28999, "in_stock": true }, "after": { "price": 27639, "in_stock": true }, "diff": null, "notified": true }
```

El webhook recibe todos los cambios; `notified` dice si la regla de la vigilancia mandó aviso push. En vigilancias de texto, `diff` trae las diferencias en formato unificado.

### MCP

`POST /mcp` (Streamable HTTP) con el mismo token. Herramientas: `list_watches`, `add_watch`, `get_watch`, `check_watch`, `remove_watch`, `extract`.

```bash
claude mcp add --transport http vigia https://tu-vigia/mcp --header "Authorization: Bearer $VIGIA_TOKEN"
```

## Relevo

Algunos sitios rechazan las IP de centros de datos: responden con un desafío anti-bots o con 403/429 aunque la página sea pública. El relevo es un Worker de Cloudflare sin estado (`relay/worker.js`, 70 líneas) que el servidor llama para pedir la página desde otra red. No necesita ninguna máquina tuya encendida y cabe de sobra en el plan gratuito.

No hay secreto compartido: cada servidor genera su propia llave, firma cada petición (ECDSA P-256, válida 2 minutos) y el Worker sólo guarda la llave pública. Para instalarlo:

1. En Vigía: Ajustes → Relevo → **Copiar código del Worker** (ya trae tu llave pública).
2. En Cloudflare: Workers → Crear → «Hello World» → Editar código, pega el código y despliega.
3. De vuelta en Vigía, pega la dirección `https://….workers.dev` en Ajustes → Relevo y guarda.

El servidor siempre intenta primero directo. Sólo si el sitio lo rechaza repite la petición por el relevo, y recuerda esa ruta para esa vigilancia. El relevo no resuelve desafíos: si el sitio también lo rechaza a él, la vigilancia queda «Bloqueado» y lo dice.

## Agentes

Otra ruta, para cuando ni el servidor ni el relevo pueden: un agente es este mismo binario corriendo en otra red (tu casa, tu laptop). Se conecta al servidor hacia afuera, sin abrir puertos, y pide las páginas desde ahí mientras esté encendido.

```bash
VIGIA_SERVER=https://tu-vigia VIGIA_TOKEN=… VIGIA_AGENT_NAME=casa vigia agent

# o con Docker
docker run -d --restart unless-stopped --name vigia-agent -e VIGIA_SERVER=https://tu-vigia \
  -e VIGIA_TOKEN=… -e VIGIA_AGENT_NAME=casa ghcr.io/elpablazo/vigia agent
```

## Lo que no hace

- No ejecuta JavaScript: las páginas que arman su contenido en el navegador llegan vacías y se reportan así.
- No resuelve CAPTCHAs ni desafíos anti-bots, y no disfraza su identidad: se presenta como `Vigia/<versión>` (el User-Agent se puede cambiar en Ajustes). Un sitio que rechaza todas las rutas configuradas se marca como «Bloqueado». Pasa, por ejemplo, con tiendas de Shopify que responden 429 a cualquier IP de servidor.
- No lee PDF: los reporta como error en vez de compararlos.
- No pasa muros de verificación de cuenta o edad (Mercado Libre, Steam los ponen a cualquier visita automatizada).
- Un solo usuario (un token). Avisos por push y webhook; para otros canales (Telegram, correo) conecta el webhook a n8n.

## Desarrollo

```bash
cargo test
VIGIA_TOKEN=dev cargo run   # http://localhost:8080
```

Código: `src/main.rs` (API, revisiones, programador), `src/extract.rs` (HTML → precio/texto), `src/fetch.rs` (peticiones, relevo y agentes), `src/history.rs` (estadísticas y compresión), `src/webpush.rs` (avisos push, RFC 8291/8292), `src/mcp.rs`, `src/ui.html` y `src/pwa/` (interfaz e instalación, sin dependencias), `relay/worker.js` (relevo). Los íconos se generan con `python3 tools/icons.py`.

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
