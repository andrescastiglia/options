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

Antes de usar `readonly` o `live`, creá una clave maestra privada fuera de `DATA_DIR`, configurá su ruta absoluta y cifrá la contraseña de IOL mediante el prompt oculto:

```bash
cargo run -- --init-master-key /ruta/privada/options-master.key
export OPTIONS_MASTER_KEY_PATH=/ruta/privada/options-master.key
cargo run -- --encrypt-password
```

El último comando imprime un valor `v3:` y termina. Copialo manualmente en `IOL_PASSWORD` dentro de `.env`. La clave se crea con permisos `0600`: no debe versionarse, copiarse a logs ni guardarse junto con snapshots. `v3:` deriva mediante HKDF-SHA256 una subclave exclusiva para la contraseña. `-e` fue retirado porque exponía el secreto en argumentos e historial. Los valores `v2:` y los antiguos ligados a `/etc/machine-id` sólo se descifran para migración en readonly; `live` exige `v3:`. Rotar la clave requiere volver a cifrar la contraseña y reemitir las autorizaciones/evidencias firmadas con la clave anterior.

```bash
cargo run
```

En una terminal interactiva, la TUI se abre únicamente después de confirmar la conexión inicial con IOL. Si ese preflight falla, los reintentos se informan en la terminal y la TUI no aparece. En CI, pipes o con `TUI_ENABLED=false`, se ejecuta en modo headless.

Si la conexión REST se pierde mientras la TUI está abierta, el motor espera `CONNECTION_RETRY_DELAY_SECS` entre intentos y prueba hasta `CONNECTION_RETRY_ATTEMPTS` veces. Si no se recupera, detiene nuevas operaciones, activa el freno operativo y mantiene una alerta roja `NO OPERATIVO` en pantalla para que el operador revise IOL manualmente.

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

Learning simula; al aprobar el gate queda `Eligible`. `MODE=live` se rechaza al iniciar si falta cualquiera de los siete gates de configuración: confirmación, endpoint contractual, paths de autorización/readiness/calendario, referencia horaria independiente o clave maestra. Antes de `Armed`, un readiness pre-canary firmado debe demostrar los gates de cobertura, branches, mutación y fuzzing del plan para el build exacto. Una autorización efímera ligada a cuenta, epoch, build, readiness, estrategia y reporte permite pasar por `Armed` a `Canary`. El grant incluye nonce y firma HMAC con la clave maestra, vence a los 15 minutos y se consume al usarlo. Canary usa límites reducidos y sólo pasa a Live tras acumular evidencia real suficiente. Una regresión o calibración vencida/cambiada bloquea entradas y vuelve a Learning al quedar plano.

Además del gate estadístico, el envío de dinero real requiere esta configuración explícita. Puede dejarse incompleta durante Learning; en ese caso el programa aprende pero no se promueve:

```bash
MODE=live
LIVE_TRADING_CONFIRMATION=I_UNDERSTAND_THIS_SENDS_REAL_ORDERS
IOL_ORDER_PATH=/api/v2/operar
LIVE_READINESS_PATH=data/live/release-readiness.json
LIVE_AUTHORIZATION_PATH=data/live/live-authorization.json
```

`release-readiness.json` se genera únicamente después de obtener reportes reales. El manifest sin firma declara el hash integral mostrado por `cargo run -- --print-build-hash`, métricas globales, todos los scopes críticos, SHA-256 de los reportes normalizados de cobertura/mutación y de un archivo del corpus fuzz revisado. La firma vuelve a comprobar los tres artefactos entregados:

```bash
cargo run -- --sign-release-readiness \
  readiness-input.json readiness-coverage.json readiness-mutation.json fuzz-evidence.tar.zst \
  data/live/release-readiness.json
```

Los mínimos son 90/85/85/80 global y 95/95/90/90 por scope crítico para líneas/regiones/branches/mutación. Un manifest sintético puede probar el parser, pero no es evidencia operativa.

Cuando aparezca `data/live/live-authorization-request.json`, revisar sus datos —incluido el hash del readiness— y emitir un grant de 15 minutos en otra invocación. El archivo se consume al usarlo y no puede reutilizarse:

```bash
cargo run -- --authorize-live data/live/live-authorization-request.json data/live/live-authorization.json
```

La utilidad verifica antes de pedir confirmación que cuenta, epoch, hashes y límites tengan forma contractual y que `build_hash` corresponda exactamente al ejecutable que firma. El resumen de terminal enmascara el número de cuenta; los valores completos permanecen únicamente en el request privado que debe revisarse.

La ruta no tiene un default deliberadamente y sólo se admite `/api/v2/operar`, verificado contra el contrato oficial IOL. Antes del `POST` real, el motor sincroniza una intención durable en el journal; un `401`, timeout o error ambiguo nunca provoca un reenvío automático. Después de aceptar una orden, exige un número de operación y sigue su detalle por REST hasta `Terminada`, `Rechazada` o `Cancelada`. Un movimiento WebSocket con el mismo número adelanta la consulta, pero nunca sustituye la confirmación REST. Si el estado no termina dentro de `ORDER_TRACKING_TIMEOUT_SECS`, solicita cancelar el remanente y espera otros `ORDER_CANCEL_TIMEOUT_SECS`. Una ejecución parcial, datos contradictorios o un resultado todavía ambiguo detienen el motor para conciliación manual.

Antes del primer tick y periódicamente, `live` reconcilia cartera y órdenes de IOL. Cada posición exige símbolo y cantidad entera positiva; cada pendiente exige además ID broker y lado reconocido. Cualquier fila ambigua invalida el snapshot completo y bloquea el motor: no se descarta ni redondea. La evidencia compatible se comparte por fingerprint bajo `DATA_DIR/evidence/<fingerprint>/`; cambiar estrategia, build o política del gate impide reutilizar evidencia incompatible.

## Captura y replay acelerado

Con `CAPTURE_MARKET_DATA=true`, cada frame validado de IOL se agrega a `DATA_DIR/market/<ticker>/<día>.jsonl`. Para reproducir un dataset a máxima velocidad, sin credenciales ni esperas de red:

Un solo proceso puede usar cada `DATA_DIR`; si necesita ejecutar instancias independientes, configure una raíz distinta para cada una. Esto serializa cuota, retención y estado compartido también entre `readonly` y `live`.

```bash
MODE=readonly REPLAY_PATH=data/market/GGAL/20689.jsonl TUI_ENABLED=false RECOVER_STATE=false cargo run
```

El bundle registra el SHA-256 del dataset. `REPLAY_PATH` debe ser no vacío; un valor blanco se considera ausente y no evita exigir credenciales IOL. Un replay ordinario queda marcado como investigación no sellada y no contribuye al gate que habilita dinero real: el hash prueba identidad de bytes, no procedencia ni ausencia de tuning. Existe un registro firmado para congelar procedencia y split cronológico, y una evaluación de holdout de un solo uso; aun así, registrar no promueve automáticamente evidencia histórica. La evidencia operativa continúa requiriendo revisión explícita y shadow prospectivo.

El flujo de sellado usa archivos privados preparados y revisados por el operador:

```bash
cargo run --locked -- --sign-dataset-manifest manifest.json signed-manifest.json
cargo run --locked -- --register-dataset dataset.jsonl signed-manifest.json data/datasets
cargo run --locked -- --consume-sealed-holdout dataset.jsonl signed-manifest.json data/datasets evaluacion-2026-08-25
```

Los tres comandos requieren `OPTIONS_MASTER_KEY_PATH`. Consumir sólo marca el holdout de forma irreversible y entrega el intervalo a la API de evaluación sellada; no autoriza `live`.

## Transporte IOL y datos de cuenta

IOL documenta actualmente WebSocket sólo para movimientos de cuenta. Está desactivado por default con `IOL_WEBSOCKET_ENABLED=false`; mercado, perfil, cartera, operaciones y órdenes permanecen sobre REST. Si se habilita, exige WSS, usa una cola acotada y reintenta errores o timeouts con backoff exponencial de 1 a 60 segundos. Como los movimientos ordinarios no contienen un identificador de orden correlacionable, el seguimiento usa REST; si excepcionalmente llega `numeroOperacion`, el evento sólo adelanta la próxima verificación REST.

El TUI separa `IOL: ONLINE/OFFLINE` de `WS: DESACTIVADO/CONECTANDO/CONECTADO/RECONECTANDO/OFFLINE`. El encabezado sólo muestra ese estado breve; las causas técnicas, descartes y reintentos quedan en `Lo que fue pasando`. Si el WebSocket está desactivado o rechaza una cuenta no habilitada, las capacidades REST continúan operativas. Una cuenta bloqueada o un saldo contractual inválido se informa como bloqueo operativo con IOL conectado, no como una falsa caída de red.

Al iniciar, los costos configurados en variables de entorno son el fallback. Después de autenticar, se consulta la última operación terminada y su detalle: los renglones de `aranceles` se separan en comisión, IVA y otros cargos, se convierten a porcentajes del monto operado y reemplazan la estimación sólo en memoria. Al finalizar se consulta nuevamente y se imprime una sugerencia lista para copiar; el programa nunca modifica `.env` automáticamente.

## Configuración principal

| Variable | Default | Propósito |
|---|---:|---|
| `MODE` | `readonly` | `readonly` o `live` |
| `TICKER` | `GGAL` | Símbolo subyacente de BCBA |
| `TUI_ENABLED` | `true` | Habilitar interfaz interactiva |
| `CONNECTION_RETRY_ATTEMPTS` | `3` | Reintentos antes de declarar el motor no operativo |
| `CONNECTION_RETRY_DELAY_SECS` | `5` | Espera fija entre reintentos de conexión |
| `CHECK_INTERVAL_SECS` | `1` | Intervalo del motor |
| `ENTRY_DELAY_AFTER_OPEN_MINS` | `45` | Minutos de observación desde la apertura durante los que no se abren posiciones |
| `WEEKEND_RISK_ENABLED` | `true` | Evitar posiciones durante fines de semana y feriados prolongados |
| `PRE_BREAK_LAST_ENTRY_TIME` | `15:00` | Corte de entradas en la víspera de una pausa |
| `PRE_BREAK_FORCE_EXIT_TIME` | `16:30` | Inicio del cierre obligatorio previo a una pausa |
| `EXPIRY_DAY_FORCE_EXIT_TIME` | `15:15` | Cierre obligatorio de series que vencen ese día |
| `LUNCH_SLOWDOWN_ENABLED` | `true` | Activar el régimen conservador de liquidez de mediodía |
| `LUNCH_SLOWDOWN_START_TIME` / `LUNCH_SLOWDOWN_END_TIME` | `12:30` / `14:00` | Ventana argentina del régimen de mediodía |
| `LUNCH_POSITION_FACTOR` | `0.5` | Factor sobre efectivo, pérdida y contratos durante el régimen |
| `LUNCH_MAX_SPREAD_FACTOR` | `0.75` | Factor sobre el spread máximo de entrada durante el régimen |
| `LUNCH_SIGNAL_THRESHOLD_BONUS` | `0.05` | Bonus del umbral si hay un meta-filtro aprendido activo |
| `POST_LUNCH_CONFIRMATION_MINS` | `5` | Pausa de entradas para reconfirmar la tendencia al salir del régimen |
| `LUNCH_LIQUIDITY_WINDOW_MINS` | `5` | Ventana de actividad exigida a la serie elegida |
| `LUNCH_MIN_QUOTE_UPDATES` | `3` | Cambios mínimos de bid, ask o volumen dentro de esa ventana |
| `MIN_SAMPLES_FOR_TREND` | `5` | Confirmación consecutiva de una señal robusta |
| `TREND_CHANGE_SAMPLES` | `3` | Confirmaciones robustas para reversión |
| `TREND_DEADBAND_PERCENTAGE` | `0.10` | Separación mínima respecto de la SMA |
| `MIN_TREND_R_SQUARED` | `0.60` | Calidad lineal mínima de la tendencia |
| `MAX_POSITION_SIZE` | `5` | Contratos por posición |
| `CONTRACT_MULTIPLIER` | `1` | Fallback paper/legado; una entrada real usa la metadata del instrumento IOL |
| `CONTRACT_MULTIPLIER_CONFIRMED` | `false` | Confirmación del fallback para conciliación; no inventa metadata real ausente |
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
| `VIX_QUOTE_URL` | — | Adaptador HTTP opcional que entrega nivel, cierre previo y timestamp del VIX |
| `VIX_MAX_AGE_SECS` | `900` | Antigüedad máxima de una cotización VIX actual |
| `VIX_PREVIOUS_CLOSE_MAX_AGE_SECS` | `345600` | Vigencia de visualización del cierre previo; nunca se usa como dato actual |
| `VIX_ELEVATED_LEVEL` | `25` | Régimen elevado candidato a reducir exposición |
| `VIX_SPIKE_CHANGE_PERCENTAGE` | `10` | Salto contra el cierre previo que eleva el umbral del meta-filtro |
| `VIX_ELEVATED_POSITION_FACTOR` | `0.5` | Factor sobre efectivo, pérdida y contratos cuando el VIX elevado está validado |
| `VIX_SPIKE_THRESHOLD_BONUS` | `0.10` | Exigencia adicional de probabilidad ante un salto validado |
| `TIME_REFERENCE_URL` | — | Fuente HTTPS de hora independiente de IOL; obligatoria en `live` |
| `TIME_REFERENCE_REFRESH_SECS` | `300` | Intervalo de verificación del reloj independiente |
| `TIME_REFERENCE_MAX_SKEW_SECS` | `30` | Desvío máximo admitido del reloj local para nuevas entradas reales |
| `MAX_MARKET_DATA_AGE_SECS` | `15` | Antigüedad máxima de cotización para decidir u operar |
| `ORDER_TRACKING_TIMEOUT_SECS` | `30` | Espera de estado terminal antes de solicitar cancelación |
| `ORDER_STATUS_POLL_INTERVAL_MILLIS` | `1000` | Intervalo REST de seguimiento de órdenes |
| `ORDER_CANCEL_TIMEOUT_SECS` | `15` | Espera de confirmación terminal después de cancelar |
| `MAX_OPTION_SPREAD_PERCENTAGE` | `3` | Spread bid/ask máximo permitido para comprar |
| `MIN_OPTION_VOLUME` | `10` | Volumen mínimo de la serie |
| `MIN_OPTION_CHAIN_ACCEPTANCE_PERCENTAGE` | `80` | Porcentaje mínimo de contratos de catálogo válidos para habilitar una entrada |
| `MIN_OPTION_CHAIN_CONTRACTS_PER_SIDE` | `1` | Mínimo de CALL y PUT válidas para habilitar una entrada |
| `LIVE_LEARNING_MIN_TRADES` | `200` | Cierres mínimos del epoch antes de evaluar el gate |
| `DATA_DIR` | `data` | Journal y snapshots por modo |
| `DATA_DIR_MAX_BYTES` | `2147483648` | Cuota agregada del árbol de datos (2 GiB) |
| `DATA_DISK_MIN_FREE_BYTES` | `536870912` | Reserva mínima del volumen (512 MiB) |
| `MARKET_CAPTURE_RETENTION_DAYS` | `30` | Retención de captures; no elimina estado ni evidencia |
| `CAPTURE_MARKET_DATA` | `true` | Guardar frames validados como JSONL |
| `HOLIDAYS_API_BASE_URL` | `https://api.argentinadatos.com/v1/feriados` | Calendario anual de feriados argentinos |
| `MARKET_SESSIONS_PATH` | — | Manifiesto BYMA versionado; obligatorio para habilitar órdenes reales |
| `REPLAY_PATH` | — | Dataset JSONL reproducido sin conectarse a IOL |
| `RECOVER_STATE` | `true` | Recuperar snapshot+journal |
| `IOL_WEBSOCKET_ENABLED` | `false` | Habilitar el canal opcional de movimientos; REST sigue siendo autoritativo |
| `IOL_WEBSOCKET_URL` | `wss://websocket-movements.invertironline.com/` | WebSocket oficial de movimientos |

La definición consolidada del algoritmo, sus precedencias y todos los defaults está en [`docs/ALGORITMO.md`](docs/ALGORITMO.md). El código autoritativo de configuración está en [`src/config.rs`](src/config.rs).

### Horario y calendario de mercado

En conexión real con IOL, el horario regular es de lunes a viernes entre las 10:30 inclusive y las 17:00 exclusive, hora argentina. `MARKET_SESSIONS_PATH` declara cierres, aperturas y horarios especiales de BYMA con fuente y hash; es obligatorio para que `live` quede listo para ordenar. ArgentinaDatos queda como señal civil auxiliar: un día civil no laborable puede negociarse si el manifiesto bursátil lo declara, y un cierre BYMA prevalece aunque no figure como feriado nacional. Sus fechas deben ser reales, únicas, pertenecer al año pedido y tener descripción. Si la fuente o su cobertura falta, `live` permanece cerrado de manera conservadora.

Una cadena de opciones degradada se informa en TUI y bloquea solamente nuevas entradas cuando no alcanza los mínimos configurados; nunca bloquea la reducción o el cierre de riesgo existente.

Durante los primeros `ENTRY_DELAY_AFTER_OPEN_MINS` minutos (45 por default), el mercado figura online y el motor recopila cotizaciones, VIX y tendencia, pero bloquea nuevas entradas. La TUI muestra `ONLINE · OBSERVANDO APERTURA` y la hora de habilitación. Las salidas de una posición recuperada siguen gestionándose. Cumplido el plazo no se reinicia el detector: se utiliza su ventana móvil reciente.

Con `WEEKEND_RISK_ENABLED=true`, el calendario busca la próxima rueda. Si está a más de un día calendario, la sesión actual se considera víspera de pausa: desde `PRE_BREAK_LAST_ENTRY_TIME` no se abren posiciones y desde `PRE_BREAK_FORCE_EXIT_TIME` se cierra al bid cualquier exposición restante. También se descartan series que venzan antes de la próxima rueda. Una serie que ya vence en la sesión actual se cierra desde `EXPIRY_DAY_FORCE_EXIT_TIME`. Si falta serie, bid o confirmación de ejecución, la posición permanece registrada y se activa un bloqueo operativo; nunca se supone una venta inexistente. La TUI distingue `PAUSA PRÓXIMA` en amarillo y `CIERRE OBLIGATORIO` en rojo.

Con `LUNCH_SLOWDOWN_ENABLED=true`, de 12:30 a 14:00 el mercado sigue online, pero las entradas usan por default la mitad del efectivo, pérdida y contratos, admiten sólo el 75 % del spread normal y exigen tres cambios reales de bid, ask o volumen durante una ventana completa de cinco minutos. Si existe un meta-filtro aprendido, su umbral aumenta 0,05. La TUI muestra `ONLINE · LIQUIDEZ DE MEDIODÍA`; las salidas nunca se bloquean. A las 14:00 se reinicia sólo la confirmación de señal y se bloquean entradas cinco minutos, con `ONLINE · RECONFIRMANDO DESPUÉS DEL MEDIODÍA`. Al reiniciar el programa dentro de la ventana, debe reconstruirse la ventana completa de actividad antes de entrar.

Fuera de rueda la TUI muestra `OFFLINE · MERCADO CERRADO` junto con el motivo y no se solicitan nuevos frames ni se abren operaciones. Al reabrir se descarta la tendencia anterior y comienza un calentamiento nuevo. Los replay continúan aislados del reloj y la red reales: Weekend Risk usa la hora grabada y la distancia hasta la siguiente sesión del dataset; si el archivo termina un viernes, asume el salto normal hasta el lunes. El régimen de mediodía también se evalúa con la hora grabada.

### VIX y validación walk-forward

El VIX no genera la dirección de la operación. Se guarda en cada frame y en el contexto de entrada, y se prueba como extensión del meta-filtro base. El reporte usa walk-forward anidado, calibración Brier, cobertura, estabilidad entre folds y concentración por dirección/sesión. La comparación con/sin VIX usa exactamente el mismo subconjunto con observaciones completas.

El modelo VIX se activa únicamente si cumple los mínimos configurados —20 aceptaciones futuras por default—, mantiene expectativa estresada positiva y supera estrictamente al modelo comparable sin VIX. Hasta entonces no altera señales ni tamaño. Una vez activado, un nivel elevado reduce los tres límites de entrada, mientras que un salto contra el cierre previo aumenta el umbral. Si falta VIX durante Learning, el motor continúa con las siete variables base; si el filtro VIX ya fue activado, una observación no vigente bloquea esa nueva entrada.

`VIX_QUOTE_URL` debe responder este contrato JSON:

```json
{"level":18.4,"previous_close":17.2,"timestamp_secs":1787662800,"previous_close_timestamp_secs":1787576400,"value_kind":"current"}
```

Puede protegerse con `VIX_BEARER_TOKEN`. El token sólo se envía como cabecera y no forma parte del estado ni del fingerprint. Los frames capturados incorporan la observación, por lo que `REPLAY_PATH` reproduce la información disponible en el momento de cada decisión sin consultar la red.

`MAX_NOTIONAL` se acepta como alias legado de `MAX_INVESTMENT_AMOUNT`. La cantidad enviada es el menor valor entre `MAX_POSITION_SIZE` y los contratos que caben en el presupuesto al precio límite, multiplicador contractual y costo operativo efectivo de compra. Antes de cada posible entrada real se consulta el estado de cuenta IOL: se exige una cuenta de inversión en pesos operable y el presupuesto se reduce al menor de `disponible` y `disponibleOperar` inmediato. Una respuesta ausente o ambigua bloquea la orden. El costo es `(COMMISSION_PERCENTAGE + OTHER_FEES_PERCENTAGE) × (1 + VAT_PERCENTAGE / 100)`.

El cálculo cobra ese costo efectivo dos veces y sobre bases distintas: una vez sobre el importe de compra y otra sobre el importe de venta. Por eso `P&L neto = bruto - costo de compra - costo de venta - impuesto`. `TAX_PERCENTAGE` se aplica solamente a una ganancia bruta positiva; el IVA ya está incluido en los costos de ambas puntas.

En `readonly` y `live`, una cotización obsoleta activa el kill switch. Una opción cuyo spread supera `MAX_OPTION_SPREAD_PERCENTAGE` no puede abrirse; el spread no impide cerrar exposición. La TUI distingue VIX `VIGENTE`, `CIERRE PREVIO`, `DESACTUALIZADO` y `NO DISPONIBLE`; sólo el primero alimenta entradas.

Las funciones cuantitativas se promueven por separado y vienen apagadas: `OPTION_ANALYTICS_ENABLED`, `IV_RANK_FILTER_ENABLED`, `ADAPTIVE_ENTRY_FILTER_ENABLED`, `VOLATILITY_NORMALIZED_SIGNALS_ENABLED`, `NONLINEAR_META_FILTER_ENABLED`, `TREE_META_FILTER_ENABLED`, `EXPERIMENT_RUNNER_ENABLED`, `DYNAMIC_LIMIT_ENABLED` y `VERTICAL_SPREAD_RESEARCH_ENABLED`. La analítica exige tasa/dividendos con timestamp y fuente punto-en-tiempo. IV Rank se recolecta por subyacente, CALL/PUT y tenor sin mirar observaciones futuras. El runner hace una comparación temporal de investigación, pero su holdout recalculado no es evidencia sellada para `live`; árbol, límite dinámico y vertical permanecen en investigación. No se agrega una ruta real de TWAP ni de órdenes combinadas.

La observabilidad queda en `DATA_DIR/telemetry/`: candidatos con motivos de descarte, IV Rank con ventana/causa de ausencia, ejecuciones con latencia/remanente y spreads verticales shadow. `baseline-report.json` segmenta resultados por lado, hora, minutos desde apertura, DTE, spread y VIX; si se habilita el runner, `experiment-report.json` guarda manifest, selección temporal y resultado final.

## Persistencia y recuperación

Los datos se guardan bajo `DATA_DIR/<modo>/`:

```text
journal.jsonl  # eventos append-only, tipados y secuenciados
state.json     # snapshot completo, versionado y escrito atómicamente
```

Al recuperar, el motor carga el snapshot, reproduce eventos posteriores y rechaza inconsistencias entre la posición del motor y el portfolio. También detecta órdenes locales sin estado final. En `live`, IOL es la fuente de verdad adicional para cartera y pendientes; símbolo, cantidad y tipo CALL/PUT deben coincidir. Una posición reconstruida congela la metadata contractual vigente del catálogo. Si no puede verificarla, conserva visible la exposición y activa el freno operativo en vez de simular una salida o habilitar entradas. `data/` está ignorado por Git.

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
