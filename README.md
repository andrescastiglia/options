# Options Trading para IOL

Motor de trading de opciones en Rust con replay determinístico, paper trading sobre datos de IOL, controles de riesgo, journal recuperable y una TUI de monitoreo.

El modo predeterminado es `replay`: nunca envía órdenes reales. `paper` consume mercado de IOL pero ejecuta órdenes en el broker simulado. `live` requiere activación explícita y un contrato de órdenes configurado por el operador.

> No es asesoramiento financiero. La estrategia incluida es simple y sirve para validar la plataforma; su rentabilidad no está demostrada.

## Flujo del producto

1. Obtiene el precio del subyacente y su cadena de opciones.
2. Calcula SMA, pendiente, volatilidad, R² y confirmación consecutiva.
3. Una suba confirmada busca una CALL; una baja confirmada busca una PUT.
4. Selecciona una opción líquida por vencimiento y cercanía del strike.
5. Calcula la cantidad que entra en el presupuesto de compra, incluida la comisión.
6. Valida pérdidas y cantidad diaria de operaciones.
7. Envía una orden limitada idempotente al broker paper o live.
8. Cierra por objetivo neto, stop-loss, reversión, timeout o cierre manual.
9. Registra eventos tipados y snapshots atómicos para recuperación.

Los precios del subyacente y de las opciones son modelos independientes; el P&L se calcula sobre la prima ejecutada de la opción y contempla el multiplicador contractual.

## Inicio rápido

```bash
cargo run
```

En una terminal interactiva se abre la TUI. En CI, pipes o con `TUI_ENABLED=false`, se ejecuta en modo headless.

Controles de la TUI:

- `q` o `Esc`: salir con snapshot y flush del journal.
- `espacio`/`p`: pausar o reanudar el procesamiento.
- `k`: activar/desactivar el kill switch.
- `c`: cerrar manualmente la posición al bid disponible.
- `s`: guardar un snapshot.

El replay sintético recorre subas, bajas y reversiones. También se puede proporcionar un dataset JSONL:

```bash
REPLAY_PATH=./fixtures/market.jsonl cargo run
```

Cada línea debe ser un `MarketFrame` con `underlying` y `options`; los tipos serializables están en `src/market.rs`.

## Modos

### Replay

```bash
MODE=replay cargo run
```

Usa un dataset local o el escenario sintético incorporado. Las órdenes pasan por `PaperBroker`, que modela slippage y respeta precios límite.

### Paper

```bash
cp .env.example .env
# completar credenciales
MODE=paper cargo run
```

Autentica contra IOL y usa su mercado, pero las órdenes siguen siendo simuladas.

### Live

Live posee tres gates independientes:

```bash
MODE=live
LIVE_TRADING_CONFIRMATION=I_UNDERSTAND_THIS_SENDS_REAL_ORDERS
IOL_ORDER_PATH=/ruta/verificada/por/el/operador
```

La ruta no tiene un default deliberadamente: debe corresponder al contrato HTTP verificado para la cuenta/API usada. Una respuesta pendiente o parcialmente ejecutada detiene el motor y activa el kill switch para evitar asumir una ejecución inexistente.

Antes del primer tick operativo, `live` consulta la cartera argentina y las operaciones pendientes de IOL. El motor sólo opera si no hay órdenes de opciones pendientes y la posición local coincide con IOL. Si IOL informa una única CALL/PUT sin estado local, la reconstruye usando cantidad y precio promedio de compra y la evalúa inmediatamente para objetivo de ganancia o stop. Cualquier ambigüedad bloquea el motor y requiere intervención.

## Configuración principal

| Variable | Default | Propósito |
|---|---:|---|
| `MODE` | `replay` | `replay`, `paper` o `live` |
| `TUI_ENABLED` | `true` | Habilitar interfaz interactiva |
| `CHECK_INTERVAL_SECS` | `1` | Intervalo del motor |
| `MIN_SAMPLES_FOR_TREND` | `5` | Confirmación de una señal |
| `TREND_CHANGE_SAMPLES` | `3` | Muestras monotónicas para reversión |
| `MAX_POSITION_SIZE` | `5` | Contratos por posición |
| `CONTRACT_MULTIPLIER` | `1` | Unidades por contrato |
| `MAX_INVESTMENT_AMOUNT` | `100000` | Efectivo máximo por compra, incluida la comisión de entrada |
| `MAX_LOSS_PER_TRADE` | `5000` | Pérdida neta máxima por operación |
| `MAX_DAILY_LOSS` | `10000` | Activa el kill switch |
| `MAX_TRADES_PER_DAY` | `20` | Límite diario |
| `STOP_LOSS_PERCENTAGE` | `15` | Stop sobre la prima de entrada |
| `PAPER_SLIPPAGE_BPS` | `5` | Slippage del broker paper |
| `MAX_MARKET_DATA_AGE_SECS` | `15` | Antigüedad máxima de cotización para decidir u operar |
| `MAX_OPTION_SPREAD_PERCENTAGE` | `20` | Spread bid/ask máximo permitido para comprar |
| `DATA_DIR` | `data` | Journal y snapshots por modo |
| `RECOVER_STATE` | `false` replay, `true` resto | Recuperar snapshot+journal |

La lista completa y sus rangos están en [`src/config.rs`](src/config.rs).

`MAX_NOTIONAL` se acepta como alias legado de `MAX_INVESTMENT_AMOUNT`. La cantidad enviada es el menor valor entre `MAX_POSITION_SIZE` y los contratos que caben en el presupuesto al precio límite, multiplicador contractual y comisión de compra.

En `paper` y `live`, una cotización del subyacente o de la opción activa que excede `MAX_MARKET_DATA_AGE_SECS` activa el kill switch y bloquea decisiones basadas en ese dato. Una opción cuyo spread porcentual sobre el precio medio supera `MAX_OPTION_SPREAD_PERCENTAGE` no puede comprarse. El spread no bloquea una venta ya justificada por objetivo o stop, porque esa operación reduce exposición.

## Persistencia y recuperación

Los datos se guardan bajo `DATA_DIR/<modo>/`:

```text
journal.jsonl  # eventos append-only, tipados y secuenciados
state.json     # snapshot completo, versionado y escrito atómicamente
```

Al recuperar, el motor carga el snapshot, reproduce eventos posteriores y rechaza inconsistencias entre la posición del motor y el portfolio. También detecta órdenes locales sin estado final. En `live`, IOL es la fuente de verdad adicional para cartera y pendientes; ante una discrepancia el comportamiento es fail-closed. `data/` está ignorado por Git.

## Calidad

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
TUI_ENABLED=false DATA_DIR=/tmp/options-trading-smoke cargo run --quiet
```

Los tests cubren señales y reversiones, selección de opciones, P&L, slippage, límites de riesgo, contratos de parsing IOL, journal/snapshot y el ciclo completo de replay.

## Estructura

```text
src/app.rs          orquestación del ciclo y recuperación
src/tui.rs          interfaz ratatui/crossterm
src/market.rs       subyacente, opciones, selección y replay
src/pattern.rs      detector de tendencia
src/trading.rs      posiciones, P&L y máquina de estados
src/risk.rs         límites y kill switch
src/broker.rs       contrato de órdenes y PaperBroker
src/iol_client.rs   OAuth, refresh, mercado, retry y órdenes gated
src/persistence.rs  journal tipado y snapshots
src/portfolio.rs    posiciones y métricas realizadas
src/config.rs       configuración validada
```

La documentación histórica detallada permanece en [`docs/`](docs/INDEX.md); ante diferencias, el código y este README describen el comportamiento ejecutable actual.
