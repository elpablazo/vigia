# Contribuir

Gracias por el interés. Vigía quiere seguir siendo chico: un binario, pocas dependencias, sin paso de compilación para la interfaz. Antes de proponer algo grande, abre un issue para platicarlo.

## Preparar el entorno

```bash
cargo test
VIGIA_TOKEN=dev cargo run   # http://localhost:8080
```

## Antes de abrir un PR

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

- Un cambio por PR, con una prueba cuando toque lógica (extracción, precios, red).
- Dependencias nuevas sólo si unas pocas líneas no lo resuelven.
- Identificadores en inglés; textos de la interfaz en español.
- Las vulnerabilidades no van en issues: ver [SECURITY.md](SECURITY.md).

Al contribuir aceptas que tu aporte se publique bajo la licencia MIT del proyecto.
