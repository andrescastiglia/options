# Especificación del algoritmo de trading

## Propósito y fuente de verdad

Este documento describe el comportamiento ejecutable del motor Rust: señal, selección de opciones, horarios, riesgo, ejecución, aprendizaje y persistencia. Los defaults son los de `Config::from_env()` en [`src/config.rs`](../src/config.rs); el entorno o un archivo `.env` puede reemplazarlos.

Cuando exista una diferencia entre documentación y código, prevalece el código. [`.env.example`](../.env.example) reproduce los defaults de código y sirve como plantilla operativa.

> Este software no garantiza rentabilidad ni constituye asesoramiento financiero. `readonly` nunca envía órdenes reales. `live` sólo puede hacerlo en las etapas Canary o Live, con fuente IOL, cuenta reconciliada y todos los controles habilitados.

> **Estado de auditoría (26 de agosto de 2026):** este contrato fue reconciliado con el runtime y sus defaults tienen verificación automática. Esa coherencia no habilita dinero real: los gates cuantitativos, campañas adversariales, shadow/canary y aprobaciones pendientes están en [`PLAN.md`](PLAN.md).

## 1. Invariantes de seguridad

El motor mantiene estas reglas en todos los modos:

- admite como máximo una posición de opciones;
- evalúa una salida existente antes de buscar una entrada nueva;
- una pausa manual bloquea entradas, pero no la gestión de salidas;
- el spread puede impedir una compra, pero nunca impide intentar reducir exposición;
- una orden real pendiente se sigue hasta un estado terminal y, si vence el plazo, se intenta cancelar; una ejecución parcial o un resultado finalmente desconocido activa un bloqueo operativo;
- una ejecución real sólo se acepta como final si IOL informa cantidad completa, precio y `broker_order_id`;
- cada entrada real exige el multiplicador informado para ese símbolo por el catálogo IOL, con fuente `iol_catalog`, instante dentro de `CACHE_TTL_SECS`, schema conocido, SHA-256 no nulo y cuerpo raw archivado. El cuerpo HTTP exacto se conserva en `DATA_DIR/catalog/<ticker>/iol-options-v1-<sha256>.json` antes de normalizarlo; una colisión o alteración bloquea la ruta real. Fuente, instante, multiplicador, schema, hash y confirmación de archivo se congelan en la posición. Metadata futura más de 300 segundos, vencida, no archivada o legado bloquea la compra. La calibración `montoOperado / (precioOperado × cantidadOperada)` y la confirmación manual sólo sirven para migración/conciliación y para no impedir una salida reductora de riesgo; nunca habilitan una entrada real sin catálogo vigente;
- una posición u orden local que no coincide con IOL bloquea el motor; no se supone ninguna ejecución;
- un `REPLAY_PATH` no vacío aísla la ejecución de IOL y nunca puede enviar dinero real, aunque `MODE=live`; vacío o sólo espacios cuenta como ausente;
- un kill switch por pérdida diaria se libera al comenzar un nuevo día argentino; uno manual u operativo requiere intervención o reconciliación.

## 2. Ciclo de decisión

En cada iteración:

1. Inicializa o mantiene la conexión y sincroniza eventos de IOL.
2. Para fuente IOL, evalúa horario, día hábil y calendario. Si el mercado está cerrado, termina la iteración sin pedir un frame.
3. Obtiene un frame, valida subyacente, cadena de opciones y orden temporal.
4. En replay, deriva Weekend Risk y Lunch Break Slowdown de los timestamps grabados.
5. Incorpora un VIX válido si existe un adaptador; opcionalmente captura el frame.
6. Actualiza actividad de cotizaciones, riesgo diario, reconciliación y detector de tendencia.
7. Si hay una posición, evalúa primero su salida.
8. Aplica cambios pendientes de etapa y promociones de Learning/Canary.
9. Si está plano y todas las puertas de entrada están abiertas, selecciona y compra una opción.
10. Persiste un snapshot del estado.

El proceso continúa consultando el horario mientras está cerrado. Por lo tanto, si se inicia antes de las 10:30, comienza a solicitar frames automáticamente al abrir la rueda; no hace falta reiniciarlo.

## 3. Horario y calendario

### 3.1 Fuente IOL

El horario regular se considera abierto de lunes a viernes entre las **10:30 inclusive y las 17:00 exclusive**, usando hora argentina. En `live`, `MARKET_SESSIONS_PATH` debe cubrir la fecha y puede declarar una apertura, cierre, rueda sin liquidación u horario especial de BYMA.

Durante una ventana potencialmente abierta se valida el año en:

```text
<HOLIDAYS_API_BASE_URL>/<año>
```

El manifiesto bursátil incluye schema, URL de fuente HTTPS, SHA-256 canónico en hexadecimal minúsculo del material de origen, fecha de consulta, vigencia inclusiva y excepciones `open`, `closed`, `special_hours` o `trading_without_settlement`. Un estado explícito de BYMA prevalece sobre el feriado civil: también puede habilitar una rueda extraordinaria de fin de semana y, al ser autoridad suficiente para esa fecha, no depende de que responda el feed civil auxiliar. La respuesta de ArgentinaDatos sólo se acepta si contiene fechas calendario reales, únicas, del año solicitado y con nombre no vacío; se almacena en `DATA_DIR/calendar/feriados-<año>.json`. Si no hay autoridad suficiente, API ni cache válidos, el mercado queda cerrado de manera conservadora y se reintenta después de cinco minutos. Un timestamp fuera del rango representable por la zona IANA también cierra el mercado; nunca se inventa una fecha civil mediante aritmética manual. Una respuesta de red válida puede usarse durante el proceso aunque falle su persistencia, pero el diagnóstico se conserva y el siguiente inicio debe volver a validarla contra la fuente.

Fuera de rueda, en fin de semana o feriado:

- la TUI muestra `OFFLINE · MERCADO CERRADO` y el motivo;
- no se solicitan frames ni se abren posiciones;
- se ocultan frame, tendencia, P&L y opción anteriores;
- al reabrir se crea un detector nuevo y debe completarse nuevamente su calentamiento.

Una posición conservada fuera de horario no puede gestionarse hasta disponer de un nuevo frame dentro de una rueda abierta.

### 3.2 Observación de apertura

Desde las 10:30 el motor recopila precios, VIX y tendencia, pero bloquea entradas durante `ENTRY_DELAY_AFTER_OPEN_MINS`. Con el default de 45 minutos, la TUI muestra `ONLINE · OBSERVANDO APERTURA` hasta las 11:15.

Al finalizar esta etapa no se borra el historial móvil. Las posiciones recuperadas sí se gestionan durante la observación.

### 3.3 Lunch Break Slowdown

Con `LUNCH_SLOWDOWN_ENABLED=true`, el intervalo `[LUNCH_SLOWDOWN_START_TIME, LUNCH_SLOWDOWN_END_TIME)` sigue online, pero endurece las entradas:

- efectivo máximo, pérdida máxima y contratos se multiplican por `LUNCH_POSITION_FACTOR`;
- el spread admisible se multiplica por `LUNCH_MAX_SPREAD_FACTOR`;
- la serie debe llevar al menos `LUNCH_LIQUIDITY_WINDOW_MINS` bajo observación y acumular `LUNCH_MIN_QUOTE_UPDATES` cambios de bid, ask o volumen dentro de esa ventana;
- si existe un meta-filtro recomendado, su umbral suma `LUNCH_SIGNAL_THRESHOLD_BONUS`, con tope `0,95`.

Con defaults, el régimen va de 12:30 a 14:00, usa 50 % de los límites, reduce el spread admisible al 75 % y exige tres cambios en cinco minutos. La TUI muestra `ONLINE · LIQUIDEZ DE MEDIODÍA` en amarillo.

El monitor de actividad no se persiste. Si el proceso se inicia o una serie reaparece después de una ausencia mayor que la ventana, debe reconstruir una ventana completa.

Al terminar el régimen se reinicia solamente la confirmación de dirección, no el historial. `POST_LUNCH_CONFIRMATION_MINS` bloquea nuevas entradas mientras se reúnen datos posteriores; con defaults, la TUI muestra `ONLINE · RECONFIRMANDO DESPUÉS DEL MEDIODÍA` hasta las 14:05. Todas las salidas continúan habilitadas.

### 3.4 Weekend Risk y vencimiento

Con `WEEKEND_RISK_ENABLED=true`, el calendario busca la próxima rueda dentro de 14 días, omitiendo fines de semana y feriados. Si no puede determinarla, la fuente IOL queda offline de manera conservadora.

Si la próxima rueda está a más de un día calendario:

- antes de `PRE_BREAK_LAST_ENTRY_TIME` todavía se permiten entradas, pero el vencimiento debe alcanzar la próxima rueda;
- desde `PRE_BREAK_LAST_ENTRY_TIME` se bloquean compras y la TUI muestra `ONLINE · PAUSA PRÓXIMA`;
- desde `PRE_BREAK_FORCE_EXIT_TIME` se intenta cerrar al bid y la TUI muestra `ONLINE · CIERRE OBLIGATORIO` en rojo.

Desde `EXPIRY_DAY_FORCE_EXIT_TIME` también se cierra una serie con `expiry_days=0`. Weekend Risk tiene precedencia cuando coinciden ambos motivos. Si falta la serie o un bid ejecutable durante un cierre obligatorio, la posición se conserva y el motor se bloquea; nunca registra una venta ficticia. Cuando la serie desaparece, el control de vencimiento puede usar el contexto congelado al abrir la posición.

Desactivar `WEEKEND_RISK_ENABLED` desactiva conjuntamente la protección de pausas y el cierre especial por vencimiento.

### 3.5 Replay

Replay no consulta reloj, feriados ni horario reales. Procesa todos los frames del archivo en orden y usa sus timestamps para:

- aplicar Lunch Break Slowdown y la reconfirmación;
- determinar el horario de los cortes Weekend Risk;
- medir la distancia hasta la próxima sesión presente en el dataset.

Si no existe una sesión posterior y el último frame es viernes, supone tres días hasta el lunes; en los demás casos supone un día. Replay no valida feriados históricos ni descarta por sí mismo frames grabados fuera de 10:30–17:00.

## 4. Validez de datos

Un frame válido exige:

- precios `last` positivos y finitos;
- ticker, símbolo, subyacente y strike coherentes;
- cuando existen bid y ask simultáneamente, ambos positivos, finitos y `bid ≤ ask`;
- timestamps del subyacente no decrecientes;
- opciones no más de 60 segundos por detrás del timestamp del subyacente cuando ambas fuentes entregan hora de mercado.

El subyacente y cada opción conservan por separado `exchange_timestamp_secs`, `received_at_secs` y `timestamp_source`. Si IOL omite la hora propia de una cotización, se usa la hora de recepción y se marca `received`; la opción nunca hereda la hora del subyacente. Cuando existe hora de mercado, una diferencia superior a 300 segundos contra la recepción o una procedencia contradictoria invalida el frame. Además, cada respuesta JSON de IOL que incluya un encabezado HTTP `Date` se compara con el reloj local y se rechaza si el desvío supera 300 segundos. La ausencia de `Date` no se inventa ni se trata como una medición independiente. Los replay antiguos se identifican como `legacy`.

En `live`, `TIME_REFERENCE_URL` es obligatoria y debe pertenecer a un origen HTTPS distinto de IOL. El motor comprueba su encabezado `Date` cada `TIME_REFERENCE_REFRESH_SECS`; al cumplirse exactamente el intervalo vuelve a consultar. Entre consultas proyecta esa referencia con un reloj monotónico para detectar inmediatamente un salto del reloj civil local. El máximo de skew es inclusivo; superarlo invalida la observación, que no se cachea, y permite que el ciclo siguiente consulte nuevamente para recuperar. Si la fuente falla o el desvío absoluto supera `TIME_REFERENCE_MAX_SKEW_SECS`, la TUI indica `reloj NO VERIFICADO` y se bloquean nuevas entradas reales. La gestión y salida de posiciones existentes continúa. Replay no consulta esta fuente.

En vivo, el subyacente debe tener una antigüedad no mayor que `MAX_MARKET_DATA_AGE_SECS`; una violación activa bloqueo operativo. Para entrar o salir, la opción también debe estar vigente. Se tolera un timestamp futuro sólo hasta ese mismo margen; el VIX tiene una tolerancia futura independiente de 300 segundos.

Una opción sin bid y ask ejecutables no puede abrirse. Una cotización ausente o vencida nunca se reemplaza por el último precio conocido para simular una ejecución.

## 5. Señal de tendencia

### 5.1 Ventana y calentamiento

La capacidad del historial es:

```text
max((PRICE_HISTORY_MINUTES × 60) / CHECK_INTERVAL_SECS,
    MIN_SAMPLES_FOR_TREND)
```

El calentamiento exige llenar toda la capacidad. Con defaults son 1.800 muestras nominales, equivalentes a 30 minutos si llega una muestra por segundo.

### 5.2 Estadísticos

Sobre el historial móvil se calculan:

```text
SMA          = media de precios
volatilidad  = desvío estándar poblacional
move_ratio   = abs(precio_actual - SMA) / volatilidad
pendiente    = pendiente de regresión lineal
pendiente_%  = cambio lineal total / SMA / tiempo transcurrido
R²           = calidad del ajuste lineal
confianza    = min(R², 1)
               × min(move_ratio / max(MIN_TREND_MOVE_VOLATILITY_RATIO, 1), 1)
```

Una muestra clasifica como suba cuando:

- terminó el calentamiento;
- `R² ≥ MIN_TREND_R_SQUARED`;
- `move_ratio ≥ MIN_TREND_MOVE_VOLATILITY_RATIO`;
- `precio_actual > SMA × (1 + TREND_DEADBAND_PERCENTAGE / 100)`;
- `pendiente_% ≥ MIN_TREND_SLOPE_PERCENT_PER_MINUTE`.

La baja es simétrica. Una clasificación neutral reinicia la confirmación. La dirección se confirma después de `MIN_SAMPLES_FOR_TREND` clasificaciones consecutivas iguales.

Una CALL sigue una señal de suba y una PUT una señal de baja. Tras operar una dirección, no se vuelve a entrar por la misma señal hasta que aparezca neutralidad o cambie la dirección. Una salida por reversión exige `TREND_CHANGE_SAMPLES` muestras que cumplan los criterios robustos en sentido opuesto y activa `REVERSAL_COOLDOWN_SECS`.

## 6. Selección de la opción

Antes de seleccionar una candidata, una nueva entrada exige que la cadena válida represente al menos `MIN_OPTION_CHAIN_ACCEPTANCE_PERCENTAGE` de los contratos del catálogo dentro de `OPTION_EXPIRY_DAYS..=OPTION_MAX_EXPIRY_DAYS` y contenga allí como mínimo `MIN_OPTION_CHAIN_CONTRACTS_PER_SIDE` CALL y PUT aceptadas. El porcentaje es `contratos_aceptados / contratos_de_catálogo × 100` para ese tenor operativo; contratos líquidos con vencimientos no seleccionables no pueden maquillar su degradación. Cada fila por vencimiento debe estar ordenada de forma estrictamente creciente y reconciliar catálogo = aceptados + faltantes + inválidos y aceptados = CALL + PUT; sus agregados deben coincidir campo por campo con el total de la cadena. Todas las sumas son verificadas: un overflow invalida el frame o el cálculo de tenor en lugar de saturarlo. La degradación bloquea entradas, pero nunca impide reducir o cerrar una posición existente. Los captures v1 sin desglose conservan el conteo global sólo para replay compatible y nunca habilitan por sí solos una ruta real.

Cada candidata debe cumplir:

- tipo CALL o PUT correspondiente a la señal;
- `OPTION_EXPIRY_DAYS ≤ expiry_days ≤ OPTION_MAX_EXPIRY_DAYS`;
- si hay pausa próxima, vencimiento suficiente para llegar a la siguiente rueda;
- `volume ≥ MIN_OPTION_VOLUME`;
- distancia porcentual absoluta entre strike y subyacente no mayor que `MAX_OPTION_MONEYNESS_DISTANCE_PERCENTAGE`;
- cada bid o ask presente debe ser finito y positivo, aun si la contraparte está ausente; para entrar se exigen ambos y `bid ≤ ask`;
- spread no mayor que `min(MAX_OPTION_SPREAD_PERCENTAGE, STOP_LOSS_PERCENTAGE / 2)` y, durante el mediodía, multiplicado además por `LUNCH_MAX_SPREAD_FACTOR`.

El spread se calcula contra el punto medio:

```text
spread_% = (ask - bid) / ((ask + bid) / 2) × 100
```

Las candidatas se ordenan por:

1. menor fricción `spread_% + 2 × costo_operativo_% + 2 × slippage_bps / 100`;
2. mayor volumen;
3. menor distancia al dinero;
4. vencimiento más cercano a `OPTION_TARGET_EXPIRY_DAYS`.

Como costo y slippage son constantes dentro de una decisión, el primer criterio equivale normalmente a elegir el menor spread.

La distancia al dinero se calcula una sola vez como `abs(strike - subyacente) / subyacente × 100`, con ambos precios finitos y positivos. La misma función se usa para filtrar, desempatar y congelar el contexto de entrada; esto evita que la selección y la auditoría informen métricas distintas.

Todas las candidatas evaluadas se escriben en segmentos diarios `DATA_DIR/telemetry/candidates/<día_de_rueda>.jsonl`, incluidas las descartadas, sus motivos, libro, fricción, vencimiento, procedencia temporal y VIX. Al cerrar operaciones se actualiza `baseline-report.json` con expectativa y tasa de acierto por lado, hora, minutos desde la apertura, vencimiento, spread y VIX.

### 6.1 Analítica americana experimental

Con `OPTION_ANALYTICS_ENABLED=true`, el motor calcula valor intrínseco y extrínseco, volatilidad implícita y delta, gamma, theta diaria, vega y rho mediante el pricer americano CRR versionado `crr-american-v2`. La suite valida CALL y PUT contra fixtures publicados por [QuantLib/Haug](https://github.com/lballabio/QuantLib/blob/master/test-suite/americanoption.cpp), convergencia al variar `OPTION_BINOMIAL_STEPS`, cota intrínseca, monotonía, estabilidad cerca del vencimiento y round-trip de IV. Entradas inválidas o una probabilidad neutral al riesgo fuera de `[0,1]` devuelven un resultado no finito y bloquean la analítica en vez de recortar o inventar valor intrínseco. Al iniciar, `analytics/pricer-validation.json` registra versión, pasos, error máximo y fallas; cualquier falla impide activar la analítica.

La tasa y el dividend yield sólo son admisibles si `OPTION_MARKET_INPUTS_OBSERVED_AT_SECS` no está en el futuro ni supera `OPTION_MARKET_INPUTS_MAX_AGE_SECS`; sus fuentes deben quedar identificadas. Al activar analítica, omitir timestamp o fuente invalida la configuración. Esto obliga a preparar inputs históricos punto-en-tiempo para replay y evita rellenarlos con información posterior.

Si IV o delta no están disponibles o quedan fuera de sus límites, o el componente extrínseco supera su porcentaje máximo de la prima, la entrada se rechaza. Las métricas se congelan en el contexto de la operación. La función está apagada por defecto.

### 6.2 IV Rank punto-en-tiempo

Cada IV válida se persiste en `DATA_DIR/analytics/iv_history.jsonl`, separada por subyacente, CALL/PUT y bucket de tenor comparable. El percentil usa exclusivamente observaciones con timestamp anterior, conserva hasta `IV_RANK_WINDOW_SESSIONS` y exige `IV_RANK_MIN_SESSIONS`. El contexto y los segmentos `telemetry/iv_rank/<día_de_rueda>.jsonl` guardan percentil, ventana, cantidad y causa de ausencia. `iv-filter-comparison.json` compara sin filtro, filtro de IV spot e IV Rank en folds futuros usando P&L estresado posterior a costos. `IV_RANK_FILTER_ENABLED=false` por default: se recolecta evidencia sin cambiar entradas. Si se activa, sólo admite `IV_RANK_MIN ≤ rank ≤ IV_RANK_MAX`; historial insuficiente es un rechazo conservador.

Con `ADAPTIVE_ENTRY_FILTER_ENABLED=true`, la fricción total no puede superar `STOP_LOSS_PERCENTAGE × MAX_FRICTION_STOP_RATIO`. Con `VOLATILITY_NORMALIZED_SIGNALS_ENABLED=true`, spread y umbral probabilístico se ajustan de forma acotada contra `TARGET_UNDERLYING_VOLATILITY_PERCENTAGE`. Ambos filtros están apagados por defecto.

## 7. Meta-filtro y VIX

### 7.1 Modelo base

Learning siempre registra siete variables de entrada cuando están disponibles:

1. spread;
2. volumen;
3. días al vencimiento;
4. distancia al dinero;
5. confianza de tendencia;
6. R²;
7. pendiente porcentual absoluta.

El meta-filtro usa tres folds walk-forward expansivos y un corte temporal interno para elegir regularización L2. Por default exige 100 ejemplos completos, 60 de entrenamiento, 20 aceptados, cobertura de 15 %, Brier no mayor que 0,25, al menos 67 % de folds positivos y concentración máxima de 85 % por dirección o sesión. Además debe mantener expectativa estresada positiva y superar la expectativa base del mismo holdout. `NONLINEAR_META_FILTER_ENABLED=true` agrega términos cuadráticos regularizados como candidato.

Los replay ordinarios se clasifican como `ResearchReplay`: conservan hash e información diagnóstica, pero `LearningState` los rechaza para el gate de dinero real. `HistoricalOutOfSample` queda reservado para evidencia revisada mediante el registro firmado; el runtime no promueve automáticamente ningún replay a esa categoría. El shadow prospectivo sí puede contribuir al gate.

El manifiesto de dataset v1 declara SHA-256 del archivo, origen, licencia, intervalo, instrumentos únicos con alfabeto contractual, zona horaria, transformaciones no vacías/duplicadas, schema de origen, instante positivo de creación y particiones con roles incompatibles (`research`, `selection`, `sealed_validation`, `shadow`, `canary`, `live`). Se firma con una subclave separada por contexto derivada de `OPTIONS_MASTER_KEY_PATH`. El registro `DATA_DIR/datasets/` vuelve a calcular el hash, rechaza firmas o schemas inválidos, cambios de split para el mismo dataset y solapamientos selection/holdout para un mismo instrumento. Cada manifiesto registrado admite como máximo 256 KiB y el registro recorre como máximo 10.000 entradas; el raw admite hasta 1 GiB. Los tickers tienen 1–32 caracteres `A-Z`, `0-9` o `.`, y los intervalos son inclusivos, por lo que `inicio == fin` es válido. `consume_sealed_holdout` valida primero que exista exactamente un holdout, un `evaluator_id` de 1–128 caracteres sin controles y un instante positivo; luego crea una marca inmutable antes de evaluar. Un segundo uso sólo se reconoce si la marca coincide exactamente; una marca corrupta, sustituida o un error de escritura distinto de `AlreadyExists` bloquea como inconsistencia.

`TREE_META_FILTER_ENABLED=true` agrega un gradient boosting determinista de stumps, con 4/8/16 rondas y learning rate 0,05/0,10/0,20. La grilla se elige sólo en pasado y se audita con Brier, cobertura y expectativa estresada futura. Sólo se recomienda si supera al mejor modelo simple por al menos `TREE_META_FILTER_MIN_IMPROVEMENT`; el tamaño de muestra nunca constituye por sí solo una garantía. Ambos candidatos complejos están apagados por default.

`EXPERIMENT_RUNNER_ENABLED=true` escribe `experiment-report.json`: compara en las mismas sesiones delay de apertura, costo/riesgo y normalización por volatilidad; calcula un split temporal 80/20 diagnóstico que puede cambiar al crecer la muestra y nunca habilita `live`. La API `run_sealed_temporal_experiment` es la única ruta que puede declarar `used_untouched_final_holdout=true`: toma selection y holdout del manifiesto firmado y quema el holdout antes de calcular el reporte. Ambos reportes incluyen expectativa neta/estresada, drawdown, cobertura, CALL/PUT, sesiones, hora y sensibilidad de costos.

El meta-filtro se aplica después de Learning. Si rechaza una señal, esa misma dirección no se reevalúa hasta neutralidad o cambio de tendencia.

### 7.2 VIX

IOL no provee VIX en el cliente implementado. Para usarlo en vivo se necesita `VIX_QUOTE_URL`; `VIX_BEARER_TOKEN` es opcional. El adaptador debe responder:

```json
{"level":18.4,"previous_close":17.2,"timestamp_secs":1787662800,"previous_close_timestamp_secs":1787576400,"value_kind":"current"}
```

Nivel y cierre previo deben ser positivos y finitos. `value_kind` distingue `current` de `previous_close`. El dato actual usa `VIX_MAX_AGE_SECS`; el cierre previo usa `VIX_PREVIOUS_CLOSE_MAX_AGE_SECS`. La TUI distingue `VIGENTE`, `CIERRE PREVIO`, `DESACTUALIZADO` y `NO DISPONIBLE`. Sólo un dato `current` vigente alimenta el algoritmo.

El candidato VIX agrega nivel y cambio porcentual a las siete variables base. Se adopta sólo si, sobre el mismo subconjunto con VIX completo, cumple los mínimos configurados —20 aceptaciones por default—, tiene expectativa estresada positiva y supera estrictamente al modelo base comparable.

Cuando el modelo VIX queda seleccionado:

- una observación VIX ausente, vencida o sin cierre previo bloquea esa entrada;
- un cambio igual o superior a `VIX_SPIKE_CHANGE_PERCENTAGE` suma `VIX_SPIKE_THRESHOLD_BONUS` al umbral;
- un nivel igual o superior a `VIX_ELEVATED_LEVEL` multiplica efectivo, pérdida y contratos por `VIX_ELEVATED_POSITION_FACTOR`.

Sin modelo VIX validado, el VIX no altera dirección ni tamaño. Un cierre previo puede mostrarse, pero nunca se trata como cotización actual. La falta de VIX no impide gestionar una posición ya abierta.

## 8. Dimensionamiento y ejecución

### 8.1 Límites efectivos

La cantidad es el menor entero permitido por:

- `MAX_POSITION_SIZE` o `CANARY_MAX_POSITION_SIZE`;
- presupuesto de compra, incluido el costo de entrada;
- pérdida neta máxima calculada por contrato;
- límites activos de la etapa;
- factores VIX y Lunch Break Slowdown.

Los factores VIX y mediodía se multiplican. Los contratos se redondean hacia abajo con mínimo uno, pero la entrada se rechaza si ese contrato no cabe en el presupuesto o en la pérdida permitida.

Antes de cada entrada con dinero real se vuelve a consultar `GET /api/v2/estadocuenta`. Debe existir una única cuenta `inversion_Argentina_Pesos`, en moneda `peso_Argentino`, con estado `operable` y un saldo de liquidación `inmediato`. El presupuesto efectivo se limita además al menor valor entre `disponible` y `disponibleOperar`; no se reutiliza durante 60 segundos el saldo de una evaluación anterior. Una respuesta ausente, ambigua, bloqueada o inválida impide enviar la orden. El contrato exacto y su procedencia están en [`DATA_CONTRACTS.md`](DATA_CONTRACTS.md).

El RiskManager comprueba además kill switch, pérdida diaria y cantidad de operaciones. La pérdida diaria es **P&L realizado neto** de operaciones cerradas durante el día argentino; no incorpora una valuación no realizada. La posición larga abierta se controla por separado con pérdida máxima congelada al entrar, stop, salida por vencimiento, liquidez, Weekend Risk y bloqueo operativo si no existe un bid ejecutable. Esta separación evita sumar dos veces el mismo riesgo, pero implica que `MAX_DAILY_LOSS` no es una medida intradía mark-to-market. `MAX_TRADES_PER_DAY` se contabiliza al cerrar cada operación; con una sola posición simultánea, el siguiente ingreso queda bloqueado una vez alcanzado el máximo.

### 8.2 Órdenes

La compra usa el ask como precio de mercado y `ask × 1,005` como límite. La venta usa el bid y `bid × 0,995` como límite.

En `readonly` y Learning se utiliza un broker simulado idempotente. La idempotencia conserva la intención completa: repetir el mismo `operation_id` con iguales campos devuelve el mismo resultado, pero reutilizarlo con símbolo, lado, cantidad o precios distintos se rechaza. El fill incorpora `READONLY_SLIPPAGE_BPS` sólo en readonly fuera de Learning; Learning y todo `MODE=live` usan `LEARNING_SLIPPAGE_BPS` para las simulaciones y estimaciones conservadoras.

En dinero real, la respuesta al envío no basta salvo que ya informe una ejecución completa. El ciclo es:

1. Escribir `order_intent_created` y sincronizar el journal a disco antes del primer `POST` real.
2. No repetir un `POST` que devolvió `401`, timeout o error ambiguo. Se renueva el token para reconciliar, pero no se reenvía la intención.
3. Persistir y sincronizar `order_accepted` tan pronto IOL informa `broker_order_id` (`numeroOperacion`).
4. Consultar `GET /api/v2/operaciones/<broker_order_id>` cada `ORDER_STATUS_POLL_INTERVAL_MILLIS`.
5. Si un movimiento WebSocket incluye exactamente el mismo `numeroOperacion`, adelantar esa consulta REST. Los movimientos sin identificador se registran, pero no se atribuyen a la orden.
6. Aceptar `Ejecutada/Terminada` únicamente con cantidad exactamente igual a la solicitada, precio finito positivo y `broker_order_id`. Un parcial exige cantidad intermedia, precio e ID; cantidades negativas, fraccionarias, ausentes, excesivas o transiciones regresivas son inconsistentes. Una vez observado, el `broker_order_id` tampoco puede cambiar ni volver a estar ausente en una actualización posterior.
7. Si no termina dentro de `ORDER_TRACKING_TIMEOUT_SECS`, hacer una última consulta y luego solicitar `DELETE /api/v2/operaciones/<broker_order_id>` para cancelar el remanente.
8. Continuar consultando hasta `ORDER_CANCEL_TIMEOUT_SECS`. Si `DELETE` falla, hacer una conciliación REST final: sólo un estado terminal confirmado puede resolver la orden; cualquier otro resultado permanece ambiguo. La cancelación sólo se considera segura cuando REST confirma un estado terminal.

Una cancelación confirmada sin fills o un rechazo liberan el estado de compra/venta sin crear ni cerrar una posición. Cualquier fill parcial, falta de `broker_order_id`, error ambiguo al enviar/cancelar o ausencia de confirmación terminal conserva el estado local, registra la orden como desconocida y activa reconciliación obligatoria. De esta forma nunca se reenvía automáticamente una orden cuyo resultado podría haber afectado dinero real.

El cliente OAuth acepta únicamente un token de acceso no vacío, `token_type=Bearer` sin distinguir mayúsculas y una expiración positiva. Si faltan 30 segundos o menos para vencer, renueva antes de usarlo. Las respuestas IOL deben declarar `application/json` o un subtipo `+json`; el límite de 8 MiB es inclusivo y se controla tanto por `Content-Length` como durante el streaming. Si existe `Date`, su desvío contra el reloj local no puede superar 300 segundos. Las lecturas de mercado reintentan sólo errores HTTP/de transporte mientras quede un intento, con espera de 250 ms que duplica hasta 16 s. Tres fallos consecutivos abren durante 300 s un circuito fail-closed, incluso cuando el tercero ocurre en el último intento; una respuesta válida reinicia el contador.

El WebSocket no es fuente autoritativa del estado de órdenes: IOL lo expone para movimientos de cuenta y el contrato habitual no trae un identificador correlacionable. Está desactivado por default; al habilitarlo exige WSS, limita frames/mensajes y usa una cola acotada con conteo de descartes. Tanto un error como un timeout de conexión aplican backoff exponencial de 1 a 60 segundos; una autenticación correcta lo reinicia a un segundo. El encabezado de la TUI muestra únicamente `WS: DESACTIVADO`, `WS: CONECTANDO`, `WS: CONECTADO`, `WS: RECONECTANDO` o `WS: OFFLINE`; los errores técnicos completos quedan en `Lo que fue pasando`. El estado REST aparece por separado como `IOL: ONLINE/OFFLINE`.

Todo texto libre destinado a tracing, TUI o stderr se acota y sanea antes de mostrarse. Credenciales y tokens se reemplazan por un mensaje neutro; cuentas, correos e identificadores externos se enmascaran. Los IDs completos sólo permanecen en estructuras privadas tipadas que son necesarias para seguimiento y conciliación.

Cada resultado se agrega a `DATA_DIR/telemetry/executions/<día_de_rueda>.jsonl` con latencia, ruta, estado WebSocket al enviar, precios, cantidades pedida/ejecutada/remanente, intentos y cancelación. La investigación vertical usa la misma segmentación en `telemetry/vertical-spreads/`. Un parcial también queda como `partial_fill_exposure` en el journal.

No existe ejecución TWAP real. La investigación de límite dinámico consume frames estrictamente posteriores y modela `pending → cancel_requested → cancelled → replacement_submitted`; jamás reemplaza antes del acuse terminal. Considera profundidad, cantidad delante en cola, fills parciales, slippage, selección adversa y tiempo expuesto. Una cancelación incierta detiene reemplazos y deja el resultado no terminal (`Pending` sin fill o `PartiallyExecuted` con fill); una cancelación confirmada queda `Cancelled` y conserva cualquier fill parcial. La cantidad ya ejecutada nunca se reenvía. `DYNAMIC_LIMIT_ENABLED` está apagada y no habilita persecución en IOL hasta validar el contrato controladamente; las métricas comparables son fill rate, parciales/no-fill, slippage y selección adversa.

### 8.3 Spreads verticales

`VERTICAL_SPREAD_RESEARCH_ENABLED=true` estudia Bull Call y Bear Put debit spreads de igual vencimiento. Cada pata registra estado, cantidad pedida/ejecutada, precio, costo, timestamp y orden broker. El recovery determinista contempla fill parcial, caída, asignación/ejercicio temprano, dividendos y serie faltante; calcula pérdida económica, margen y obligación transitoria de efectivo. Una CALL corta sin cobertura se representa con pérdida no acotada y bloquea promoción; una PUT corta se acota por strike y aun así debe superar los controles de efectivo y margen.

La comparación long-only/vertical usa la misma señal y sesión e incluye todos los costos. Como el contrato IOL integrado no demuestra atomicidad ni permisos de combos, el alcance sigue siendo `shadow_only`; `VERTICAL_ATOMIC_EXECUTION_VERIFIED=false` y no existe ruta secuencial real. Cambiar la variable no basta: también debe existir soporte atómico informado por el broker.

## 9. Costos, stop, objetivo y salidas

### 9.1 P&L

El costo operativo efectivo por punta es:

```text
costo_operativo_% = (COMMISSION_PERCENTAGE + OTHER_FEES_PERCENTAGE)
                     × (1 + VAT_PERCENTAGE / 100)
```

Con defaults resulta `0,2299 %`. Para `q` contratos y multiplicador `m`:

```text
unidades      = q × m
bruto         = (precio_salida - precio_entrada) × unidades
costo_compra  = precio_entrada × unidades × costo_operativo_% / 100
costo_venta   = precio_salida  × unidades × costo_operativo_% / 100
impuesto      = max(bruto, 0) × TAX_PERCENTAGE / 100
P&L_neto      = bruto - costo_compra - costo_venta - impuesto
```

Al autenticar, el motor intenta calibrar comisión, IVA y otros cargos con la última operación de opciones terminada en IOL. La calibración debe corresponder a una opción, no tener más de 30 días y no adelantarse más de 300 segundos. Si el cambio modifica el bucket de costo incluido en el fingerprint, invalida la identidad estadística y hace volver cualquier etapa posterior a Learning cuando queda plana.

La evidencia estresada resta al P&L neto una comisión completa adicional y un slippage adicional de salida. Este estrés sólo afecta evaluación y aprendizaje; no cambia el P&L contable mostrado.

### 9.2 Stop y objetivo congelados

Al abrir se congelan costos, impuestos, slippage, stop, objetivo y pérdida máxima de esa posición.

El stop toma el precio más conservador entre:

- caída `STOP_LOSS_PERCENTAGE` desde la prima de entrada;
- precio necesario para no superar la pérdida neta máxima, incluyendo costos y slippage.

El objetivo exige simultáneamente:

- cubrir el costo estimado de ida y vuelta multiplicado por `MIN_PROFIT_MULTIPLIER`;
- alcanzar `MIN_REWARD_RISK_RATIO` sobre la pérdida neta máxima.

### 9.3 Precedencia de salida

Con una cotización vigente y bid ejecutable, la precedencia es:

1. cierre obligatorio Weekend Risk;
2. cierre obligatorio por vencimiento;
3. stop porcentual, stop congelado o pérdida máxima;
4. objetivo de precio y ganancia neta;
5. reversión robusta;
6. timeout.

Un cierre manual usa el último bid disponible y requiere que no exista una reconciliación bloqueada. Un precio inválido no se inventa para ejecutar una salida: un dato vencido bloquea el motor y un bid ausente impide enviar la orden.

## 10. Riesgo diario, evidencia y etapas

### 10.1 Gate de Learning

La secuencia es:

```text
Learning → Eligible → Armed → Canary → Live
```

- **Learning:** operaciones simuladas y generación de evidencia.
- **Eligible:** gate estadístico aprobado y calibración vigente. En readonly es la etapa terminal.
- **Armed:** autorización efímera validada y consumida.
- **Canary:** órdenes reales con límites Canary.
- **Live:** órdenes reales con límites completos.

El gate exige:

- mínimos configurados de cierres totales, CALL, PUT y sesiones argentinas;
- P&L neto positivo agregado y por lado;
- P&L estresado positivo agregado y por lado;
- profit factor mínimo agregado, CALL y PUT;
- expectativa neta positiva;
- límites inferiores bootstrap 95 % positivos para P&L y múltiplo R de CALL y PUT;
- drawdown diario no mayor que `MAX_DAILY_LOSS`;
- drawdown total no mayor que `2 × MAX_DAILY_LOSS`.

El bootstrap usa 1.000 remuestras por bloques de sesión. Las métricas de mediodía —cantidad y expectativa estresada dentro y fuera del régimen— son diagnósticas y no forman parte del gate.

### 10.2 Autorización y Canary

Para ingresar a dinero real se requiere además:

- fuente IOL y cuenta sin posiciones u órdenes ambiguas;
- calibración de costos vigente;
- `IOL_ORDER_PATH=/api/v2/operar` exacto;
- confirmación textual exacta;
- readiness v2 firmado, no mayor a 30 días, asociado al hash integral del código, lockfile y toolchain, a los reportes normalizados exactos de cobertura/mutación y al archivo del corpus de una campaña fuzz;
- gates 90/85/85/80 global y 95/95/90/90 en cada scope crítico para líneas, regiones, branches y mutation score, respectivamente;
- archivo de autorización compatible, vigente por no más de 15 minutos y asociado a cuenta, epoch, build, readiness, fingerprint y reporte;
- nonce no vacío y firma HMAC válida calculada con la clave maestra local.

El build hash incluye todos los archivos Rust bajo `src/` —una prueba obliga a registrar cualquier archivo nuevo—, `Cargo.toml`, `Cargo.lock` y `rust-toolchain.toml`, con nombres y longitudes en la entrada del digest. El readiness se firma con una subclave HMAC propia; `--sign-release-readiness` vuelve a calcular el SHA-256 de los dos reportes normalizados y del archivo de corpus suministrados, y compara las métricas declaradas con los reportes. La autorización v3 incorpora el SHA-256 del readiness ya firmado y se mueve atómicamente a un archivo consumido al pasar a Armed. Canary y Live revalidan en cada ciclo el mismo readiness autorizado y cada compra real lo comprueba otra vez inmediatamente antes del write-ahead/POST; si falta, vence, cambia o deja de verificar, bloquean entradas y vuelven a Learning al quedar planos. Este control no impide una venta reductora de riesgo. Canary usa sus propios límites incluso después de recuperar un snapshot.

Canary pasa a Live sólo después de sus mínimos de cierres, CALL, PUT y sesiones, P&L estresado positivo agregado y por lado, y ausencia de regresión.

### 10.3 Regresión

Fuera de Learning se programa un regreso a Learning al quedar plano cuando:

- se alcanza la pérdida diaria activa —Canary o Live según la etapa—;
- se acumulan `LIVE_MAX_CONSECUTIVE_LOSSES` pérdidas consecutivas;
- sobre `LIVE_REGRESSION_WINDOW_TRADES`, la expectativa neta es no positiva, el profit factor cae por debajo de 1 o el drawdown supera dos veces la pérdida diaria activa;
- vence la calibración de costos o cambia el bucket incluido en el fingerprint;
- se pierde la configuración necesaria para órdenes reales.

Los límites diarios y cantidad de operaciones Canary también se restauran correctamente al reiniciar.

## 11. Persistencia y recuperación

El journal append-only se guarda en `DATA_DIR/<modo>/journal.jsonl`; el snapshot atómico, en `DATA_DIR/<modo>/state.json`. Un lock raíz impide que dos procesos compartan el mismo `DATA_DIR`, incluso si declaran modos distintos; el lock adicional por modo protege su estado específico. Los directorios sensibles se crean con `0700` y los archivos con `0600`; symlinks y archivos privados con permisos de grupo/otros se rechazan.

Las lecturas fallan antes de parsear si exceden sus cuotas: snapshot 64 MiB, journal e historial IV 128 MiB, manifiesto bursátil/calendario 4 MiB, manifiesto registrado de dataset 256 KiB y replay o raw de dataset 1 GiB, con un máximo adicional de 1.000.000 de frames para replay. Los límites son inclusivos: se acepta exactamente la cuota y se rechaza el byte siguiente; en el journal se cuentan el evento serializado y su salto de línea. Los frames capturados tienen un tope de 256 MiB por archivo. Tipo, tamaño y permisos se vuelven a comprobar sobre el descriptor abierto para evitar sustituciones entre validación y lectura.

Antes de iniciar y antes de cada ciclo se mide el árbol completo de `DATA_DIR`, sin seguir symlinks y con límites de profundidad y cantidad de entradas. El procesamiento se detiene de forma conservadora si la escritura prevista superaría `DATA_DIR_MAX_BYTES` o invadiría la reserva `DATA_DISK_MIN_FREE_BYTES`; no se envían órdenes si no puede preservarse estado durable. Al iniciar sólo se eliminan captures de mercado cuyo día de sesión excede `MARKET_CAPTURE_RETENTION_DAYS`. Journal, snapshots, órdenes, calendario, analytics, telemetría y evidencia nunca se eliminan automáticamente.

Al recuperar:

- se carga el snapshot y se reproducen eventos posteriores;
- en replay se omiten frames ya procesados;
- los límites de riesgo se reconstruyen desde la configuración actual y la etapa recuperada;
- cada estado de orden debe pertenecer a una intención previa. El replay vuelve a validar la ejecución contra la solicitud original y exige que `broker_order_id`, cantidad ejecutada y transición sean monotónicos; un estado huérfano, una intención modificada o un parcial mal contabilizado abortan la recuperación;
- sólo `Executed` con cantidad exacta, `Rejected` o `Cancelled` sin fills resuelven una orden local. Un resultado desconocido, estado no terminal o cancelación con fill parcial conserva la exposición pendiente y obliga a reconciliar;
- motor y portfolio deben representar exactamente cero o una misma posición. Repetir una apertura idéntica es idempotente; una segunda exposición, una posición con distinto contenido o un cierre dirigido a otro `operation_id` abortan el replay;
- liberar un `KillSwitch` durante replay sólo reanuda el motor si el gestor de riesgo también puede reanudarse. Un freno operativo no conciliado conserva el motor detenido;
- en live se compara la única posición local admitida con cartera y órdenes IOL;
- cada fila IOL de cartera debe tener símbolo y cantidad entera positiva; cada pendiente requiere además ID broker y lado reconocido. Una fila ambigua invalida el snapshot completo: nunca se descarta, redondea ni completa con cero;
- una posición existente sólo coincide si símbolo, cantidad y tipo CALL/PUT concuerdan. Si debe reconstruirse desde IOL, el tipo tampoco puede contradecir al catálogo vigente. Cuando el catálogo aporta metadata íntegra y fresca, su multiplicador, fuente, instante, schema, hash y estado de archivo se congelan en la posición recuperada. Si esa metadata no puede verificarse, la exposición permanece visible con un fallback conservador para valuación, pero se activa un freno operativo: no se simula una salida ni se habilitan entradas nuevas;
- una identidad de estrategia incompatible inicia un nuevo epoch de Learning.

La evidencia reside en `DATA_DIR/evidence/<fingerprint>/`. El fingerprint incluye schema, build, parámetros de estrategia, costos operativos agrupados en escalones de 0,05 puntos porcentuales y política del gate. Los frames capturados se guardan como JSONL en `DATA_DIR/market/<ticker>/`.

Cada operación conserva el contexto de entrada, incluido VIX, régimen de mediodía, liquidez, IV y Greeks. El snapshot vigente es v4 y migra v1/v2/v3. El journal admite v1–v6: readonly continúa escribiendo la cadena SHA-256 v5, mientras `live` exige v6 y autentica cada evento con HMAC derivado de una subclave separada de `OPTIONS_MASTER_KEY_PATH`. La cadena detecta truncamiento/reordenamiento y el HMAC impide recomputarla válidamente sin la clave; una instalación v5 se ancla al primer evento v6. Además, todo evento de orden, fill o posición exige que el `operation_id` exterior coincida exactamente con el ID de su payload tipado; la inconsistencia se rechaza antes de escribir y también al leer schemas legados. Los captures nuevos usan envelope v1 con fuente normalizada, hora de captura y SHA-256 del frame; replay también lee frames legados sin envelope, pero éstos no adquieren procedencia por esa compatibilidad. Cambiar schema, build o un parámetro del fingerprint inicia un nuevo epoch y obliga a revalidar Learning → Canary → Live.

| Artefacto | Schema vigente | Lectura compatible | Escritura/migración |
|---|---:|---|---|
| Snapshot | v4 | v1–v4 | siempre v4; v1–v3 se normalizan al cargar |
| Journal | v6 | v1–v6 | v5 encadenado en readonly; v6 HMAC en live |
| Evidencia | v6 | sólo v6 | v6; incompatible inicia otro epoch |
| Autorización | v3 | sólo v3 | v3 efímera y de un solo uso |
| Readiness pre-canary | v2 | sólo v2 | v2 firmado y ligado al build; v1 se rechaza |
| Analytics | v2 | sólo v2 | v2 |
| Historial IV | v1 | sólo v1 | v1 |
| Experimentos | v1 | sólo v1 | v1 diagnóstico o sellado explícito |
| Capture de mercado | v1 | frame legado o envelope v1 | todo capture nuevo usa v1 |
| Manifiesto de dataset | v1 | sólo v1 | v1 firmado e inmutable en el registro |
| Manifiesto bursátil | v1 | sólo v1 | v1; no se migra implícitamente |

## 12. Variables de entorno y defaults

### 12.1 Proceso, datos y conexión

| Variable | Default | Validación y efecto |
|---|---:|---|
| `MODE` | `readonly` | `readonly` o `live`; `live` falla al iniciar si cualquiera de sus siete gates de configuración está ausente. |
| `TICKER` | `GGAL` | Símbolo BCBA de 1–12 bytes: mayúsculas ASCII, dígitos o punto. |
| `CHECK_INTERVAL_SECS` | `1` | Intervalo entre iteraciones; 1–60 s. |
| `TUI_ENABLED` | `true` | Habilita la interfaz si existe terminal. |
| `LOG_LEVEL` | `info` | `debug`, `info`, `warn` o `error`. |
| `RUST_LOG` | valor de `LOG_LEVEL` | Filtro de logging Rust; no pertenece a `Config`. |
| `CONNECTION_RETRY_ATTEMPTS` | `3` | Reintentos de conexión; 1–20. |
| `CONNECTION_RETRY_DELAY_SECS` | `5` | Espera fija entre reintentos; 1–300 s. |
| `MAX_CONCURRENT_REQUESTS` | `10` | Solicitudes IOL concurrentes; 1–100. |
| `CACHE_TTL_SECS` | `60` | TTL del catálogo de opciones; 1–86.400 s. |
| `DATA_DIR` | `data` | Raíz de estado, evidencia, calendario y capturas. |
| `DATA_DIR_MAX_BYTES` | `2147483648` | Cuota agregada de `DATA_DIR`; 64 MiB–1 TiB. |
| `DATA_DISK_MIN_FREE_BYTES` | `536870912` | Espacio libre que debe quedar después de la escritura prevista; 0–1 TiB. |
| `MARKET_CAPTURE_RETENTION_DAYS` | `30` | Días conservados de captures; 1–3.650. Sólo esta clase se poda automáticamente. |
| `RECOVER_STATE` | `true` | Recupera snapshot, journal y evidencia compatible. |
| `CAPTURE_MARKET_DATA` | `true` | Captura frames IOL validados en JSONL. |
| `REPLAY_PATH` | sin default | Dataset JSONL; sólo un valor no vacío omite la exigencia de credenciales IOL. |
| `HOLIDAYS_API_BASE_URL` | `https://api.argentinadatos.com/v1/feriados` | Debe usar HTTPS; el año se agrega al final. |
| `MARKET_SESSIONS_PATH` | sin default | Manifiesto bursátil versionado; obligatorio para armar órdenes reales. |

### 12.2 Horarios y regímenes

| Variable | Default | Validación y efecto |
|---|---:|---|
| `ENTRY_DELAY_AFTER_OPEN_MINS` | `45` | Observación desde 10:30; 0–390 min. |
| `WEEKEND_RISK_ENABLED` | `true` | Activa pausa prolongada y cierre por vencimiento. |
| `PRE_BREAK_LAST_ENTRY_TIME` | `15:00` | Debe cumplir 10:30 ≤ corte < cierre forzoso. |
| `PRE_BREAK_FORCE_EXIT_TIME` | `16:30` | Posterior al corte y anterior a 17:00. |
| `EXPIRY_DAY_FORCE_EXIT_TIME` | `15:15` | Entre 10:30 inclusive y 15:30 exclusive. |
| `LUNCH_SLOWDOWN_ENABLED` | `true` | Activa el régimen de mediodía. |
| `LUNCH_SLOWDOWN_START_TIME` | `12:30` | Inicio inclusivo, no anterior a 10:30. |
| `LUNCH_SLOWDOWN_END_TIME` | `14:00` | Fin exclusivo, posterior al inicio. |
| `LUNCH_POSITION_FACTOR` | `0.5` | Factor de efectivo, pérdida y contratos; 0,01–1. |
| `LUNCH_MAX_SPREAD_FACTOR` | `0.75` | Factor del spread máximo; 0,01–1. |
| `LUNCH_SIGNAL_THRESHOLD_BONUS` | `0.05` | Bonus del meta-filtro; 0–0,4. |
| `POST_LUNCH_CONFIRMATION_MINS` | `5` | Reconfirmación; 0–120 min y debe terminar antes de 17:00. |
| `LUNCH_LIQUIDITY_WINDOW_MINS` | `5` | Ventana de actividad; 1–60 min. |
| `LUNCH_MIN_QUOTE_UPDATES` | `3` | Cambios mínimos en la ventana; 1–1.000. |

Las horas de Weekend y Lunch se validan aunque la política correspondiente esté desactivada.

### 12.3 Tendencia y opciones

| Variable | Default | Validación y efecto |
|---|---:|---|
| `PRICE_HISTORY_MINUTES` | `30` | Ventana nominal; 1–120 min. |
| `MIN_SAMPLES_FOR_TREND` | `5` | Confirmaciones consecutivas; 2–100. |
| `TREND_CHANGE_SAMPLES` | `3` | Muestras de reversión robusta; 2–100. |
| `TREND_DEADBAND_PERCENTAGE` | `0.10` | Banda respecto de SMA; 0–10 %. |
| `MIN_TREND_SLOPE_PERCENT_PER_MINUTE` | `0.02` | Pendiente mínima absoluta; 0–100 %/min. |
| `MIN_TREND_R_SQUARED` | `0.60` | R² mínimo; 0–1. |
| `MIN_TREND_MOVE_VOLATILITY_RATIO` | `1.0` | Distancia mínima en volatilidades; 0–100. |
| `REVERSAL_COOLDOWN_SECS` | `300` | Espera tras salida por reversión; entero sin rango adicional. |
| `OPTION_EXPIRY_DAYS` | `1` | Días mínimos al vencimiento. |
| `OPTION_TARGET_EXPIRY_DAYS` | `21` | Vencimiento preferido para desempate. |
| `OPTION_MAX_EXPIRY_DAYS` | `45` | Días máximos al vencimiento. |
| `MIN_OPTION_VOLUME` | `10` | Volumen mínimo, mayor que cero. |
| `MIN_OPTION_CHAIN_ACCEPTANCE_PERCENTAGE` | `80` | Porcentaje mínimo de contratos del catálogo aceptados dentro del tenor operable; 1–100 %. |
| `MIN_OPTION_CHAIN_CONTRACTS_PER_SIDE` | `1` | Mínimo de contratos válidos CALL y PUT antes de una entrada; 1–10.000. |
| `MAX_OPTION_SPREAD_PERCENTAGE` | `3` | Spread máximo; 0,1–100 %. |
| `MAX_OPTION_MONEYNESS_DISTANCE_PERCENTAGE` | `10` | Distancia máxima al dinero; 0,1–100 %. |
| `MAX_MARKET_DATA_AGE_SECS` | `15` | Antigüedad y tolerancia futura; 1–300 s. |
| `OPTION_ANALYTICS_ENABLED` | `false` | Activa IV/Greeks y sus filtros; experimental. |
| `OPTION_RISK_FREE_RATE` | `0.35` | Tasa anual decimal del árbol; −0,5–5. |
| `OPTION_DIVIDEND_YIELD` | `0` | Dividend yield anual decimal; 0–2. |
| `OPTION_MARKET_INPUTS_OBSERVED_AT_SECS` | sin default | Timestamp punto-en-tiempo; obligatorio si la analítica está activa. |
| `OPTION_MARKET_INPUTS_MAX_AGE_SECS` | `86400` | Vigencia máxima de tasa/dividendos; 1–31.536.000 s. |
| `OPTION_RISK_FREE_SOURCE` | `manual_env` | Identificador no vacío de la fuente de tasa. |
| `OPTION_DIVIDEND_SOURCE` | `manual_env` | Identificador no vacío de la fuente de dividendos. |
| `OPTION_BINOMIAL_STEPS` | `150` | Pasos del árbol americano; 25–2.000. |
| `OPTION_MIN_ABS_DELTA` / `OPTION_MAX_ABS_DELTA` | `0.15` / `0.85` | Rango de delta absoluto; 0–1. |
| `OPTION_MIN_IMPLIED_VOLATILITY` / `OPTION_MAX_IMPLIED_VOLATILITY` | `0.01` / `3` | Rango de IV decimal. |
| `OPTION_MAX_EXTRINSIC_PERCENTAGE` | `100` | Máximo valor extrínseco como porcentaje de la prima; 0–100. |
| `IV_RANK_FILTER_ENABLED` | `false` | Recolecta siempre; sólo al activarlo filtra por percentil histórico. |
| `IV_RANK_WINDOW_SESSIONS` | `252` | Sesiones previas conservadas; 2–2.000. |
| `IV_RANK_MIN_SESSIONS` | `60` | Cobertura temporal mínima; 2–ventana. |
| `IV_RANK_MIN` / `IV_RANK_MAX` | `0` / `100` | Rango de percentil admisible. |
| `ADAPTIVE_ENTRY_FILTER_ENABLED` | `false` | Activa el filtro fricción/riesgo. |
| `MAX_FRICTION_STOP_RATIO` | `0.25` | Fracción máxima del stop consumida por fricción; 0,01–1. |
| `VOLATILITY_NORMALIZED_SIGNALS_ENABLED` | `false` | Ajusta spread y umbral por régimen. |
| `TARGET_UNDERLYING_VOLATILITY_PERCENTAGE` | `1` | Volatilidad porcentual de referencia. |
| `VERTICAL_SPREAD_RESEARCH_ENABLED` | `false` | Registra spreads verticales shadow; nunca reales. |
| `VERTICAL_ATOMIC_EXECUTION_VERIFIED` | `false` | Evidencia de atomicidad; por sí sola no crea una ruta real. |

Debe cumplirse `OPTION_EXPIRY_DAYS ≤ OPTION_TARGET_EXPIRY_DAYS ≤ OPTION_MAX_EXPIRY_DAYS`.

### 12.4 Costos, posición y riesgo

| Variable | Default | Validación y efecto |
|---|---:|---|
| `COMMISSION_PERCENTAGE` | `0.19` | Comisión neta por punta; 0–10 %. |
| `VAT_PERCENTAGE` | `21` | IVA sobre comisión y otros cargos; 0–100 %. |
| `OTHER_FEES_PERCENTAGE` | `0` | Otros cargos netos por punta; 0–10 %. |
| `TAX_PERCENTAGE` | `35` | Impuesto estimado sobre bruto positivo; 0–100 %. |
| `MIN_PROFIT_MULTIPLIER` | `2.0` | Múltiplo de costos exigido; 1–10. |
| `MIN_REWARD_RISK_RATIO` | `1.25` | Beneficio neto objetivo/riesgo; 1–10. |
| `MAX_INVESTMENT_AMOUNT` | `100000` | Presupuesto máximo positivo. |
| `MAX_NOTIONAL` | alias de `MAX_INVESTMENT_AMOUNT` | Sólo se usa si la variable nueva falta. |
| `MAX_LOSS_PER_TRADE` | `5000` | Pérdida máxima positiva por operación. |
| `MAX_DAILY_LOSS` | `10000` | Pérdida diaria positiva que activa kill switch. |
| `MAX_TRADES_PER_DAY` | `20` | Cierres máximos contabilizados; mayor que cero. |
| `MAX_POSITION_SIZE` | `5` | Contratos máximos; mayor que cero. |
| `CONTRACT_MULTIPLIER` | `1` | Fallback mayor que cero para paper/legado; una entrada real exige metadata del instrumento. |
| `CONTRACT_MULTIPLIER_CONFIRMED` | `false` | Confirmación manual de fallback para conciliación; no reemplaza metadata ausente en una entrada real. |
| `STOP_LOSS_PERCENTAGE` | `15` | Caída máxima de prima; 0,1–100 %. |
| `POSITION_TIMEOUT_MINS` | `60` | Permanencia máxima; mayor que cero. |
| `READONLY_SLIPPAGE_BPS` | `5` | Slippage readonly fuera de Learning; 0–1.000 bps. |
| `LEARNING_SLIPPAGE_BPS` | `25` | Slippage Learning y `MODE=live`; 0–1.000 bps. |

### 12.5 VIX

| Variable | Default | Validación y efecto |
|---|---:|---|
| `VIX_QUOTE_URL` | sin default | Adaptador HTTP(S); sin URL no se consulta VIX en vivo. |
| `VIX_BEARER_TOKEN` | sin default | Token Bearer opcional y no persistido. |
| `VIX_REFRESH_SECS` | `60` | Intervalo mínimo de consulta; 1–86.400 s. |
| `VIX_MAX_AGE_SECS` | `900` | Antigüedad máxima de una cotización actual; 60–604.800 s. |
| `VIX_PREVIOUS_CLOSE_MAX_AGE_SECS` | `345600` | Antigüedad máxima para mostrar un cierre previo; no lo vuelve dato actual. |
| `VIX_ELEVATED_LEVEL` | `25` | Umbral de nivel elevado; 5–100. |
| `VIX_SPIKE_CHANGE_PERCENTAGE` | `10` | Cambio mínimo contra cierre previo; 0,1–100 %. |
| `VIX_ELEVATED_POSITION_FACTOR` | `0.5` | Factor de límites; 0,01–1. |
| `VIX_SPIKE_THRESHOLD_BONUS` | `0.10` | Bonus de probabilidad; 0–0,4. |

### 12.6 Learning, regresión y Canary

| Variable | Default | Validación y efecto |
|---|---:|---|
| `LIVE_LEARNING_MIN_TRADES` | `200` | Cierres mínimos totales. |
| `LIVE_LEARNING_MIN_CALL_TRADES` | `75` | Cierres CALL mínimos. |
| `LIVE_LEARNING_MIN_PUT_TRADES` | `75` | Cierres PUT mínimos. |
| `LIVE_LEARNING_MIN_SESSIONS` | `20` | Sesiones argentinas mínimas. |
| `LIVE_LEARNING_MIN_PROFIT_FACTOR` | `1.25` | Mínimo agregado y por lado; 1–10. |
| `META_FILTER_MIN_EXAMPLES` | `100` | Ejemplos completos mínimos; 30–100.000. |
| `META_FILTER_MIN_TRAIN_EXAMPLES` | `60` | Entrenamiento mínimo y menor que el total. |
| `META_FILTER_MIN_ACCEPTED_HOLDOUT` | `20` | Aceptaciones futuras mínimas. |
| `META_FILTER_MIN_COVERAGE` | `0.15` | Cobertura mínima del holdout; 0–1. |
| `META_FILTER_MAX_BRIER_SCORE` | `0.25` | Error probabilístico máximo; 0–1. |
| `META_FILTER_MIN_POSITIVE_FOLD_RATIO` | `0.67` | Proporción mínima de folds rentables; 0–1. |
| `META_FILTER_MAX_CONCENTRATION` | `0.85` | Concentración máxima por lado o sesión; 0–1. |
| `NONLINEAR_META_FILTER_ENABLED` | `false` | Habilita el candidato cuadrático regularizado. |
| `TREE_META_FILTER_ENABLED` | `false` | Habilita el candidato boosting acotado. |
| `TREE_META_FILTER_MIN_IMPROVEMENT` | `0.05` | Mejora mínima de expectativa estresada sobre el mejor modelo simple. |
| `EXPERIMENT_RUNNER_ENABLED` | `false` | Genera una comparación temporal diagnóstica; el holdout no está sellado ni habilita `live`. |
| `LIVE_REGRESSION_WINDOW_TRADES` | `30` | Ventana reciente, mayor que cero. |
| `LIVE_MAX_CONSECUTIVE_LOSSES` | `3` | Pérdidas consecutivas, mayor que cero. |
| `CANARY_MIN_TRADES` | `20` | Cierres Canary mínimos. |
| `CANARY_MIN_CALL_TRADES` | `5` | Cierres CALL Canary. |
| `CANARY_MIN_PUT_TRADES` | `5` | Cierres PUT Canary. |
| `CANARY_MIN_SESSIONS` | `5` | Sesiones Canary mínimas. |
| `CANARY_MAX_POSITION_SIZE` | `1` | Contratos Canary; 1–límite Live. |
| `CANARY_MAX_INVESTMENT_AMOUNT` | `10000` | Presupuesto Canary positivo y no mayor que Live. |
| `CANARY_MAX_LOSS_PER_TRADE` | `500` | Pérdida por operación positiva y no mayor que Live. |
| `CANARY_MAX_DAILY_LOSS` | `1000` | Pérdida diaria positiva y no mayor que Live. |
| `CANARY_MAX_TRADES_PER_DAY` | `5` | Cierres diarios, entre 1 y el límite Live. |

Los mínimos CALL más PUT no pueden superar el mínimo total, tanto en Learning como en Canary.

### 12.7 IOL y autorización real

| Variable | Default | Validación y efecto |
|---|---:|---|
| `IOL_USERNAME` | requerido sin replay | Usuario IOL. |
| `OPTIONS_MASTER_KEY_PATH` | sin default | Ruta absoluta a una clave maestra de 32 bytes y permisos `0600`; es obligatoria para `live`. |
| `IOL_PASSWORD` | requerido sin replay | Se genera con `--encrypt-password`; `live` exige el formato autenticado `v3:` derivado mediante HKDF-SHA256. |
| `IOL_REFRESH_TOKEN` | cadena vacía | Refresh token inicial opcional. |
| `IOL_BASE_URL` | `https://api.invertironline.com` | Base REST de IOL. |
| `TIME_REFERENCE_URL` | sin default | Origen HTTPS independiente de IOL; obligatorio para `live`. |
| `TIME_REFERENCE_REFRESH_SECS` | `300` | Refresco de la verificación; rango 30–3.600 s. |
| `TIME_REFERENCE_MAX_SKEW_SECS` | `30` | Desvío local máximo; rango 1–300 s. Un exceso bloquea entradas reales. |
| `IOL_WEBSOCKET_ENABLED` | `false` | Habilita el canal opcional de movimientos; no cambia la autoridad REST. |
| `IOL_WEBSOCKET_URL` | `wss://websocket-movements.invertironline.com/` | Debe usar `wss://` cuando está habilitado. |
| `IOL_ORDER_PATH` | sin default | Debe ser exactamente `/api/v2/operar`; cualquier otra ruta o su ausencia impide dinero real. |
| `ORDER_TRACKING_TIMEOUT_SECS` | `30` | Segundos para esperar un estado terminal antes de cancelar; rango 1–300. |
| `ORDER_STATUS_POLL_INTERVAL_MILLIS` | `1000` | Intervalo de seguimiento REST; rango 100–10.000 ms. |
| `ORDER_CANCEL_TIMEOUT_SECS` | `15` | Segundos para confirmar el estado final luego del `DELETE`; rango 1–120. |
| `DYNAMIC_LIMIT_ENABLED` | `false` | Identifica el estudio frame-aware en manifest/fingerprint; no altera la ruta operativa. |
| `DYNAMIC_LIMIT_STEPS` | `4` | Escalones simulados hasta el límite; 1–20. |
| `DYNAMIC_LIMIT_FRAME_WAIT_SECS` | `2` | Separación objetivo entre frames de investigación; 1–60 s. |
| `DYNAMIC_LIMIT_QUEUE_AHEAD_FACTOR` | `1` | Supuesto conservador de prioridad de cola; 0–100. |
| `DYNAMIC_LIMIT_ADVERSE_SELECTION_BPS` | `10` | Estrés adicional para comparación; 0–1.000 bps. |
| `LIVE_READINESS_PATH` | sin default | Readiness v2 firmado y ligado al build/reportes; obligatorio para Canary. |
| `LIVE_AUTHORIZATION_PATH` | sin default | Archivo de autorización efímera. |
| `LIVE_TRADING_CONFIRMATION` | sin default | Debe ser exactamente `I_UNDERSTAND_THIS_SENDS_REAL_ORDERS`. |

Las variables opcionales de path o URL vacías o compuestas sólo por espacios cuentan como ausentes. `IOL_USERNAME` e `IOL_PASSWORD` también deben contener al menos un carácter no blanco, y la confirmación de `live` no admite espacios adicionales ni variantes de mayúsculas. La clave se inicializa una sola vez con `--init-master-key RUTA` y debe permanecer fuera del repositorio y de `DATA_DIR`. El prompt de `--encrypt-password` no muestra ni recibe la contraseña por argumentos y rechaza un secreto vacío. El formato vigente `v3:` deriva una subclave de contraseña con HKDF-SHA256 y contexto versionado. `v2:` y el descifrado basado en `/etc/machine-id` existen sólo para migrar instalaciones readonly y no habilitan dinero real. La clave maestra deriva además subclaves separadas por contexto para autorización efímera, readiness, journal y manifiestos de datasets; cualquier alteración invalida la firma. Antes de solicitar la frase de confirmación, `--authorize-live` valida schema, cuenta, epoch, hashes SHA-256 canónicos, límites Canary y coincidencia con el build ejecutable. El resumen de terminal enmascara la cuenta; el request privado conserva el valor exacto para la revisión del operador. Rotar la clave obliga a volver a cifrar la contraseña y reemitir los artefactos firmados; no existe fallback automático a una clave anterior en `live`.

## 13. Constantes internas y defaults derivados

| Concepto | Valor |
|---|---:|
| Hora argentina | Zona IANA `America/Argentina/Buenos_Aires` |
| Apertura / cierre | 10:30 / 17:00 |
| Búsqueda de próxima rueda | 14 días |
| Reintento de calendario | 5 min |
| Timeout calendario | conexión 5 s; total 10 s |
| Margen de compra / venta | +0,5 % / −0,5 % |
| Reconciliación periódica IOL | 60 s |
| Vigencia de calibración | 30 días |
| Tolerancia futura VIX/calibración | 300 s / 300 s |
| Desvío máximo hora de opción/recepción | 300 s |
| Autorización real máxima | 15 min |
| Variables meta base / VIX | 7 / 9 |
| Ejemplos evaluación / entrenamiento | 100 / 60 |
| Folds walk-forward | 3 |
| Candidatos L2 / learning rate / iteraciones | 0,001; 0,01; 0,1 / 0,05 / 1.000 |
| Umbral base / tope con bonus | 0,55 / 0,95 |
| Aceptaciones futuras mínimas | 20 |
| Remuestras bootstrap | 1.000 |

Con defaults:

- historial: `30 × 60 / 1 = 1.800` muestras;
- habilitación horaria inicial: 11:15, aunque además debe terminar el calentamiento y confirmarse la señal;
- costo operativo por punta: `(0,19 + 0) × 1,21 = 0,2299 %`;
- spread normal: `min(3 %, 15 % / 2) = 3 %`;
- spread de mediodía: `3 % × 0,75 = 2,25 %`;
- umbral meta de mediodía: `0,55 + 0,05 = 0,60`, antes de un posible bonus VIX;
- límites con VIX elevado y mediodía simultáneos: `0,5 × 0,5 = 25 %`;
- drawdown total máximo del gate: `2 × 10000 = 20000`;
- víspera de pausa: corte de entradas a las 15:00 y cierre obligatorio desde las 16:30;
- serie que vence: cierre obligatorio desde las 15:15.
