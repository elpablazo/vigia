# Seguridad

## Reportar una vulnerabilidad

No abras un issue público. Usa **Security → Report a vulnerability** en este repositorio (reporte privado de GitHub). Incluye la versión, cómo reproducirlo y el impacto. Recibirás respuesta en unos días; las correcciones se publican como una versión nueva y se acreditan si quieres.

Sólo se da soporte a la última versión publicada.

## Modelo de seguridad

Vigía es de un solo usuario: quien tiene el token puede todo. Lo que sí defiende:

- **Acceso.** Toda la API y `/mcp` exigen el token (`Authorization: Bearer` o la cookie de sesión `HttpOnly; SameSite=Strict`, con `Secure` detrás de HTTPS). La comparación es de tiempo constante. Sin `VIGIA_TOKEN` se genera uno aleatorio por ejecución; nunca arranca abierto.
- **SSRF.** Sólo `http`/`https`. Se rechazan destinos que resuelven a redes privadas, loopback, link-local o CGNAT, también en cada redirección. `VIGIA_ALLOW_PRIVATE=1` lo desactiva.
- **Contenido remoto.** Las páginas vigiladas son datos no confiables: el tamaño de descarga, la profundidad del árbol HTML y el texto guardado tienen tope, y la interfaz escapa todo lo que muestra. La página se sirve con CSP, `X-Frame-Options: DENY` y `Referrer-Policy: no-referrer`.
- **Contenedor.** Imagen `scratch` con un binario estático que corre sin privilegios (UID 65534). No hay shell ni paquetes del sistema.
- **Cadena de suministro.** `Cargo.lock` fijo y compilación con `--locked`; acciones de CI fijadas por hash; Dependabot para crates y acciones; los binarios publicados llevan suma SHA-256 y atestación de procedencia.

## Límites conocidos

- La defensa contra SSRF resuelve el nombre antes de conectar; un DNS que cambie de respuesta entre ambas consultas (rebinding) podría evadirla. Importa si expones Vigía a usuarios que no son tú.
- El relevo es un punto público en internet que pide cualquier URL http(s) a quien presente su token. Trata `RELAY_TOKEN` como una contraseña y rótalo si se filtra.
- Un agente ejecuta las peticiones que le manda el servidor al que se conecta. Conéctalo sólo a un servidor tuyo y por HTTPS.
- El webhook se envía a la dirección que configures, incluidas redes privadas (es intencional, para servicios internos).
- No hay límite de intentos de inicio de sesión más allá de una pausa por intento fallido: usa un token largo y aleatorio (`openssl rand -hex 24`).
