# Options Trading para IOL

Motor de señales y trading de opciones en Rust conectado a IOL, con controles de riesgo, journal recuperable y una TUI de monitoreo.

Sólo hay dos modos operativos. `readonly` nunca envía órdenes: aprende, simula resultados y avisa cuándo debería comprar o vender. `live` usa el ciclo `Learning → Eligible → Armed → Canary → Live`; sólo Canary y Live pueden enviar órdenes reales después de aprobar todos los gates.

> No es asesoramiento financiero. La estrategia incluida es simple y sirve para validar la plataforma; su rentabilidad no está demostrada.

## Flujo del producto

1. Obtiene el precio del subyacente y su cadena de opciones.
2. Calcula SMA, pendiente, volatilidad, R² y confirmación consecutiva.
3. Una suba confirmada busca una CALL; una baja confirmada busca una PUT.
4. Filtra spread, volumen, vencimiento, distancia al dinero y frescura; luego elige la serie de menor fricción.
5. Calcula la cantidad que entra simultáneamente en el presupuesto y en la pérdida neta máxima.
6. Valida pérdidas y cantidad diaria de operaciones.
7. Muestra la acción en readonly o envía una orden limitada exclusivamente en `MODE=live` y etapa Canary/Live.
8. Cierra por objetivo neto, stop-loss, reversión, timeout o cierre manual.
9. Registra eventos tipados y snapshots atómicos para recuperación.
10. Calibra comisión, IVA y otros aranceles con la última operación terminada de IOL.

Los precios del subyacente y de las opciones son modelos independientes; el P&L se calcula sobre la prima ejecutada de la opción y contempla el multiplicador contractual.

## Inicio rápido

Antes de usar `readonly` o `live`, cifrá la contraseña de IOL en la misma máquina donde se ejecutará el programa:

```bash
cargo run -- -e "tu contraseña de IOL"
```

El comando imprime un único texto Base64 y termina. Copialo manualmente en `IOL_PASSWORD` dentro de `.env`. El valor queda ligado a `/etc/machine-id`, por lo que hay que generarlo nuevamente al cambiar de máquina.

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

## Modos

### Readonly

```bash
cp .env.example .env
# completar credenciales
MODE=readonly cargo run
```

Autentica contra IOL y usa mercado real. Learning genera operaciones virtuales para reunir evidencia. Cuando aprueba el gate pasa a `Eligible`, donde continúa simulando y muestra `debería COMPRAR` o `debería VENDER` en consola/TUI. El enrutador de readonly nunca invoca el endpoint de órdenes.

### Live

Learning simula; al aprobar el gate queda `Eligible`. Una autorización efímera ligada a cuenta, epoch, build, estrategia y reporte permite pasar por `Armed` a `Canary`. Canary usa límites reducidos y sólo pasa a Live tras acumular evidencia real suficiente. Una regresión o calibración vencida/cambiada bloquea entradas y vuelve a Learning al quedar plano.

Además del gate estadístico, el envío de dinero real requiere estas dos variables. Pueden dejarse comentadas durante Learning; en ese caso el programa aprende pero no se promueve:

```bash
MODE=live
LIVE_TRADING_CONFIRMATION=I_UNDERSTAND_THIS_SENDS_REAL_ORDERS
IOL_ORDER_PATH=/ruta/verificada/por/el/operador
LIVE_AUTHORIZATION_PATH=data/live/live-authorization.json
```

Cuando aparezca `data/live/live-authorization-request.json`, revisar sus datos y emitir un grant de 15 minutos en otra invocación. El archivo se consume al usarlo y no puede reutilizarse:

```bash
cargo run -- --authorize-live data/live/live-authorization-request.json data/live/live-authorization.json
```

La ruta no tiene un default deliberadamente: debe corresponder al contrato HTTP verificado para la cuenta/API usada. Una respuesta pendiente o parcialmente ejecutada detiene el motor y activa el kill switch para evitar asumir una ejecución inexistente.

Antes del primer tick y periódicamente, `live` reconcilia cartera y órdenes de IOL. Cualquier ambigüedad bloquea el motor. La evidencia compatible se comparte por fingerprint bajo `DATA_DIR/evidence/<fingerprint>/`; cambiar estrategia, build o política del gate impide reutilizar evidencia incompatible.

## Captura y replay acelerado

Con `CAPTURE_MARKET_DATA=true`, cada frame validado de IOL se agrega a `DATA_DIR/market/<ticker>/<día>.jsonl`. Para reproducir un dataset a máxima velocidad, sin credenciales ni esperas de red:

```bash
MODE=readonly REPLAY_PATH=data/market/GGAL/20689.jsonl TUI_ENABLED=false RECOVER_STATE=false cargo run
```

El bundle registra el SHA-256 del dataset y las operaciones de replay quedan marcadas como evidencia histórica fuera de muestra. Para una validación honesta, reservar sesiones cronológicamente futuras para replay y no ajustar parámetros mirando ese tramo.

## Transporte IOL y datos de cuenta

IOL documenta actualmente WebSocket sólo para movimientos de cuenta. En ambos modos la aplicación abre `IOL_WEBSOCKET_URL`; mercado, perfil, cartera, operaciones y órdenes permanecen sobre REST.

El TUI muestra número de cuenta comitente, nombre y apellido obtenidos de `/api/v2/datos-perfil`, además del estado del WebSocket. Si el WebSocket rechaza una cuenta no habilitada para ese servicio, el estado se informa y las capacidades REST continúan operativas.

Al iniciar, los costos configurados en variables de entorno son el fallback. Después de autenticar, se consulta la última operación terminada y su detalle: los renglones de `aranceles` se separan en comisión, IVA y otros cargos, se convierten a porcentajes del monto operado y reemplazan la estimación sólo en memoria. Al finalizar se consulta nuevamente y se imprime una sugerencia lista para copiar; el programa nunca modifica `.env` automáticamente.

## Configuración principal

| Variable | Default | Propósito |
|---|---:|---|
| `MODE` | `readonly` | `readonly` o `live` |
| `TICKER` | `GGAL` | Símbolo subyacente de BCBA |
| `TUI_ENABLED` | `true` | Habilitar interfaz interactiva |
| `CHECK_INTERVAL_SECS` | `1` | Intervalo del motor |
| `MIN_SAMPLES_FOR_TREND` | `30` | Confirmación consecutiva de una señal robusta |
| `TREND_CHANGE_SAMPLES` | `5` | Confirmaciones robustas para reversión |
| `TREND_DEADBAND_PERCENTAGE` | `0.10` | Separación mínima respecto de la SMA |
| `MIN_TREND_R_SQUARED` | `0.60` | Calidad lineal mínima de la tendencia |
| `MAX_POSITION_SIZE` | `5` | Contratos por posición |
| `CONTRACT_MULTIPLIER` | `1` | Unidades por contrato |
| `COMMISSION_PERCENTAGE` | `0.19` | Comisión neta estimada; se calibra con la última operación |
| `VAT_PERCENTAGE` | `21` | IVA estimado sobre comisión y otros aranceles netos |
| `OTHER_FEES_PERCENTAGE` | `0` | Derechos de mercado y otros cargos netos sobre el monto |
| `TAX_PERCENTAGE` | `35` | Impuesto estimado sobre ganancia positiva; no representa IVA |
| `MAX_INVESTMENT_AMOUNT` | `100000` | Efectivo máximo por compra, incluida la comisión de entrada |
| `MAX_LOSS_PER_TRADE` | `5000` | Pérdida neta máxima por operación |
| `MAX_DAILY_LOSS` | `10000` | Activa el kill switch |
| `MAX_TRADES_PER_DAY` | `20` | Límite diario |
| `STOP_LOSS_PERCENTAGE` | `15` | Stop sobre la prima de entrada |
| `MIN_REWARD_RISK_RATIO` | `1.25` | Beneficio neto objetivo mínimo respecto del riesgo neto |
| `READONLY_SLIPPAGE_BPS` | `5` | Slippage de señales virtuales en etapa Live de readonly |
| `LEARNING_SLIPPAGE_BPS` | `25` | Slippage conservador en Learning |
| `MAX_MARKET_DATA_AGE_SECS` | `15` | Antigüedad máxima de cotización para decidir u operar |
| `MAX_OPTION_SPREAD_PERCENTAGE` | `3` | Spread bid/ask máximo permitido para comprar |
| `MIN_OPTION_VOLUME` | `10` | Volumen mínimo de la serie |
| `LIVE_LEARNING_MIN_TRADES` | `200` | Cierres mínimos del epoch antes de evaluar el gate |
| `DATA_DIR` | `data` | Journal y snapshots por modo |
| `CAPTURE_MARKET_DATA` | `true` | Guardar frames validados como JSONL |
| `REPLAY_PATH` | — | Dataset JSONL reproducido sin conectarse a IOL |
| `RECOVER_STATE` | `true` | Recuperar snapshot+journal |
| `IOL_WEBSOCKET_URL` | `wss://websocket-movements.invertironline.com/` | WebSocket oficial de movimientos |

La lista completa y sus rangos están en [`src/config.rs`](src/config.rs).

`MAX_NOTIONAL` se acepta como alias legado de `MAX_INVESTMENT_AMOUNT`. La cantidad enviada es el menor valor entre `MAX_POSITION_SIZE` y los contratos que caben en el presupuesto al precio límite, multiplicador contractual y costo operativo efectivo de compra. Ese costo es `(COMMISSION_PERCENTAGE + OTHER_FEES_PERCENTAGE) × (1 + VAT_PERCENTAGE / 100)`.

El cálculo cobra ese costo efectivo dos veces y sobre bases distintas: una vez sobre el importe de compra y otra sobre el importe de venta. Por eso `P&L neto = bruto - costo de compra - costo de venta - impuesto`. `TAX_PERCENTAGE` se aplica solamente a una ganancia bruta positiva; el IVA ya está incluido en los costos de ambas puntas.

En `readonly` y `live`, una cotización obsoleta activa el kill switch. Una opción cuyo spread supera `MAX_OPTION_SPREAD_PERCENTAGE` no puede abrirse; el spread no impide cerrar exposición.

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

Los tests cubren señales y reversiones, selección de opciones, P&L, slippage, límites de riesgo, contratos REST/WebSocket de IOL, calibración de aranceles, journal/snapshot y el ciclo completo de replay.

## Estructura

```text
src/app.rs          orquestación del ciclo y recuperación
src/tui.rs          interfaz ratatui/crossterm
src/market.rs       subyacente, opciones, selección y replay
src/pattern.rs      detector de tendencia
src/trading.rs      posiciones, P&L y máquina de estados
src/risk.rs         límites y kill switch
src/broker.rs       contrato de órdenes y PaperBroker
src/iol_client.rs   WebSocket de movimientos y fallback REST por capacidad
src/persistence.rs  journal tipado y snapshots
src/portfolio.rs    posiciones y métricas realizadas
src/config.rs       configuración validada
```

La documentación histórica detallada permanece en [`docs/`](docs/INDEX.md); ante diferencias, el código y este README describen el comportamiento ejecutable actual.
