# Instrucciones del proyecto

## Contexto

Este repositorio define un sistema automatico de trading de opciones para Invertir Online (IOL), escrito en Rust. La documentacion de referencia esta en:

- `README.md`: entrada rapida, alcance y comandos basicos.
- `docs/ALGORITMO.md`: especificacion funcional canonica, precedencias y defaults.
- `docs/PLAN.md`: auditoria vigente de divergencias y plan de remediacion.
- `docs/ARCHITECTURE.md`, `docs/IMPLEMENTATION_DETAILS.md`, `docs/DEPLOYMENT.md` y
  `docs/EXECUTIVE_SUMMARY.md`: vistas tecnicas consolidadas; `ALGORITMO.md` conserva precedencia.
- `docs/INDEX.md`: indice y referencias cruzadas.

Antes de cambiar comportamiento, revisar el codigo, `docs/ALGORITMO.md` y `docs/PLAN.md`.
No tomar una afirmacion documental como implementada sin verificarla en el codigo y sus tests.

## Reglas generales de implementacion

- Usar Rust estable y Tokio para I/O asincrono.
- Mantener separacion clara entre `config`, `market`, `pattern`, `trading`, `portfolio`, `persistence` y `utils`.
- Preferir tipos fuertes, errores explicitos y APIs pequenas y testeables.
- No agregar una base de datos por defecto. El estado principal vive en memoria y se recupera mediante journal append-only y snapshots locales.
- Mantener la maquina de estados explicita y evitar transiciones implicitas o estados parcialmente actualizados.
- Hacer operaciones criticas idempotentes y conservar un identificador unico por orden.
- No introducir dependencias externas sin justificar su necesidad y su impacto operacional.
- Mantener cambios pequenos, compatibles con la estructura documentada y sin refactors no relacionados.

## Seguridad y modo de operacion

- El modo predeterminado debe ser `readonly`: nunca enviar ordenes reales sin una activacion explicita.
- El modo real requiere `MODE=live`, credenciales validas y una confirmacion operacional clara.
- Nunca hardcodear credenciales, tokens, contrasenas ni secretos.
- No versionar `.env`, snapshots con secretos ni logs que contengan tokens o numeros de orden completos.
- Leer credenciales desde variables de ambiente o un gestor de secretos; usar `zeroize` para valores sensibles en memoria cuando corresponda.
- Usar HTTPS y validar certificados.
- Enmascarar datos sensibles en logs y no registrar `access_token` ni `refresh_token`.
- Ante cambios de autenticacion, cubrir expiracion, refresh, `401`, timeout y refresh token invalido.

## Configuracion

- Validar toda la configuracion al iniciar y fallar de forma clara ante valores invalidos.
- Aplicar defaults solo a parametros no sensibles; las credenciales no deben tener valores por defecto.
- Rangos documentados: `CHECK_INTERVAL_SECS` 1..60, `PRICE_HISTORY_MINUTES` 1..120, `MIN_SAMPLES_FOR_TREND` 2..100, `COMMISSION_PERCENTAGE` 0.01..1.0, `TAX_PERCENTAGE` 0..100 y `MIN_PROFIT_MULTIPLIER` 1.0..10.0.
- Usar `LOG_LEVEL` con valores `debug`, `info`, `warn` o `error`.
- Interpretar `COMMISSION_PERCENTAGE=0.19` y `TAX_PERCENTAGE=35` como porcentajes humanos; convertir a fraccion con `/ 100` al calcular.
- No existe hot reload; un cambio de configuracion requiere reinicio y nueva validacion de gates.

## Mercado y datos

- El cliente IOL debe encapsular autenticacion OAuth, refresh automatico, rate limiting, retry exponencial y circuit breaker.
- Validar cada precio: debe ser positivo, tener timestamp valido y no retroceder sin una razon explicita.
- Validar la consistencia `bid <= ask` cuando ambos datos existan; conservar el ultimo valor valido si la respuesta es inconsistente.
- Registrar gaps, saltos anormales y datos faltantes sin ocultarlos.
- Usar un buffer de ventana deslizante acotado por ticker y cache con TTL configurable.
- No consultar strikes en cada ciclo si el cache sigue siendo valido.

## Deteccion de tendencias

- La tendencia debe basarse en muestras recientes, SMA y confirmacion consecutiva.
- Usar tolerancia neutral de aproximadamente 0.1% alrededor de la SMA para evitar ruido.
- Confirmar una tendencia solo despues de `MIN_SAMPLES_FOR_TREND` muestras en la misma direccion.
- Detectar una inversion usando `TREND_CHANGE_SAMPLES` muestras opuestas.
- Diferenciar siempre `SUBA`, `BAJA`, `NEUTRA`, tendencia parcial y tendencia confirmada.
- Calcular pendiente, volatilidad y R2 cuando el contrato del detector lo requiera.
- Probar casos de subida clara, bajada, lateralidad, volatilidad, muestras insuficientes y reversal.

## Trading, riesgo y P&L

- Una tendencia confirmada de subida busca CALL; una tendencia confirmada de baja busca PUT.
- Seleccionar opciones por vencimiento configurado y strike cercano al precio subyacente, verificando bid, ask y liquidez.
- Usar orden limitada y confirmar el estado real de la orden antes de crear o cerrar una posicion.
- Las salidas validas son: ganancia neta alcanzada, cambio de tendencia, timeout o condicion defensiva documentada.
- No asumir que una orden fue ejecutada por haber sido enviada. Hacer polling y manejar pendiente, ejecutada, rechazada y timeout.
- Calcular:
  - `ganancia_bruta = (precio_venta - precio_compra) * contratos * multiplicador`
  - `comision = (precio_compra + precio_venta) * contratos * multiplicador * (commission / 100)`
  - `impuesto = max(ganancia_bruta, 0) * (tax / 100)`
  - `ganancia_neta = ganancia_bruta - comision - impuesto`
  - `threshold = comision * MIN_PROFIT_MULTIPLIER`
- Mantener consistencia entre el calculo de entrada, salida, comisiones, impuestos y journal.
- Respetar `MAX_POSITION_SIZE` y `POSITION_TIMEOUT_MINS`; nunca abrir posiciones duplicadas por reintentos.
- Rechazar decisiones basadas en cotizaciones obsoletas y bloquear compras con spread bid/ask superior al límite configurado.
- No bloquear una venta que reduce exposición solamente por spread amplio; sí bloquearla si la cotización es obsoleta.
- En una falla durante el cierre, reintentar con backoff, alertar y dejar trazabilidad para intervencion manual.

## Persistencia, recuperacion y auditoria

- El estado en memoria debe ser thread-safe y tener limites claros.
- El journal es append-only y debe registrar timestamp, id de operacion, accion, detalles y confirmacion.
- Los snapshots JSON deben escribirse atomicamente; actualmente no se comprimen.
- Al iniciar, cargar el snapshot mas reciente, reprocesar el journal desde el ultimo identificador y validar posiciones contra IOL.
- No declarar recuperacion exitosa si existe una discrepancia no resuelta entre estado local, journal y broker.
- Mantener snapshots, journal y datos operacionales fuera del control de versiones.

## Observabilidad y operacion

- Usar `tracing` con logs profesionales en niveles `info`, `warn` y `error`; reservar `debug` para diagnostico.
- Registrar latencia de API, reconexiones, refresh de token, tendencias detectadas, ordenes, P&L y eventos de recuperacion.
- Exponer o conservar metricas para uptime, latencia, tasa de ejecucion, P&L, memoria, CPU y tamano del journal.
- Implementar graceful shutdown: detener nuevas entradas, resolver o marcar posiciones activas, guardar snapshot y hacer flush del journal.
- Mantener retry exponencial y circuit breaker sin bloquear el runtime asincrono. No afirmar que existe un health endpoint.
- Cualquier cambio de deployment debe seguir `docs/DEPLOYMENT.md` y preservar permisos restrictivos de `.env` y `data/`.

## Testing y validacion

Antes de considerar terminado un cambio:

1. Ejecutar `cargo fmt --check`.
2. Ejecutar `cargo clippy --all-targets --all-features -- -D warnings` cuando el proyecto compile con Cargo.
3. Ejecutar `cargo test` y agregar tests para el comportamiento cambiado.
4. Para integraciones IOL, usar un mock server; no enviar ordenes reales desde tests.
5. Cubrir errores de red, rate limit, refresh, ordenes rechazadas, timeouts, datos invalidos, replay de journal y recuperacion de snapshots.
6. Verificar que readonly y replay siguen sin ejecutar compras o ventas reales.

Cuando el repositorio aun no tenga `Cargo.toml` o `src/`, tratar los documentos como especificacion y no inventar resultados de compilacion o tests. Mantener la documentacion alineada con la implementacion real y actualizar `docs/INDEX.md` si cambian las secciones o referencias.

## Alcance y criterio de cambios

- Priorizar seguridad, consistencia de estado y auditabilidad por encima de microoptimizaciones.
- No presentar el sistema como asesoramiento financiero ni asumir rentabilidad.
- Mantener paper trading o simulacion hasta validar comportamiento, limites y recuperacion.
- Si una solicitud contradice estas reglas, senalar el conflicto y pedir una decision explicita antes de implementar una operacion real o una relajacion de seguridad.
