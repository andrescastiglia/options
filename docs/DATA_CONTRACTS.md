# Contratos de datos externos

No se deben inventar campos, timestamps, hashes ni sesiones para superar una validación. Un dato ausente desactiva la capacidad dependiente o hace fallar cerrado `live`.

## Calendario bursátil

`MARKET_SESSIONS_PATH` referencia un JSON v1 mantenido por el operador:

```json
{
  "schema_version": 1,
  "source_url": "https://fuente-oficial.example/calendario",
  "source_sha256": "64-caracteres-hex-del-documento-fuente",
  "retrieved_at_secs": 1787680000,
  "valid_from": "2026-01-01",
  "valid_until": "2026-12-31",
  "sessions": [
    {"date":"2026-12-31","status":"closed","name":"Cierre informado por BYMA"},
    {"date":"2026-12-24","status":"special_hours","open":"10:30","close":"13:00","name":"Horario especial"}
  ]
}
```

El ejemplo ilustra el schema, no certifica esas fechas. Estados admitidos: `open`, `closed`, `special_hours` y `trading_without_settlement`. Las fechas deben ser reales, únicas y quedar dentro de la vigencia inclusiva. `source_url` exige HTTPS y `source_sha256` debe ser exactamente 64 dígitos hexadecimales minúsculos que identifiquen el artefacto fuente. En `live`, ausencia de cobertura bloquea la rueda. ArgentinaDatos aporta feriados civiles y cache por año, pero una excepción bursátil explícita tiene precedencia: puede abrir una sesión extraordinaria de fin de semana sin consultar el feed civil. Un timestamp no representable por `America/Argentina/Buenos_Aires` falla cerrado.

## VIX

El adaptador controlado por el operador responde `application/json`:

```json
{
  "level": 18.4,
  "previous_close": 17.2,
  "timestamp_secs": 1787662800,
  "previous_close_timestamp_secs": 1787576400,
  "value_kind": "current"
}
```

`timestamp_secs` identifica el nivel actual. `previous_close_timestamp_secs` es independiente, debe ser anterior y estar dentro de `VIX_PREVIOUS_CLOSE_MAX_AGE_SECS`. Un cierre previo vencido se descarta sin ocultar un nivel actual vigente. `value_kind=previous_close` nunca se presenta como actual. HTTPS es obligatorio salvo loopback HTTP para un adaptador local; el body máximo es 64 KiB.

## Referencia horaria

`TIME_REFERENCE_URL` debe ser un origen HTTPS distinto de `IOL_BASE_URL` y responder un encabezado HTTP `Date` RFC 2822. Se solicita sin caché y no se aceptan redirects. Entre refrescos, el instante remoto se proyecta sólo con tiempo monotónico y se vuelve a comparar en cada ciclo, de modo que un salto del reloj civil local no espera al siguiente GET. En `live`, un error de red, encabezado ausente/inválido o desvío absoluto superior a `TIME_REFERENCE_MAX_SKEW_SECS` bloquea nuevas entradas hasta una observación válida posterior; nunca se completa ni corrige el reloj local con datos inventados. La respuesta de IOL sigue verificándose como defensa adicional, pero no sustituye esta fuente independiente.

## Readiness pre-canary

El manifest v2 contiene `build_hash`, commit, instante, métricas globales, exactamente los scopes críticos requeridos, SHA-256 de los reportes normalizados de cobertura y mutación, SHA-256 del archivo del corpus fuzz y duración de campaña. Los scopes obligatorios son `app::order_recovery`, `build_identity`, `config`, `data_contracts`, `iol_client`, `main::authorization`, `market_calendar`, `persistence`, `release_readiness`, `risk`, `secrets`, `secure_fs`, `time_reference` y `vix`. Los normalizadores rechazan branches no instrumentadas, módulos ausentes y módulos sin mutantes viables; un scope compuesto toma el peor porcentaje de sus módulos. Los porcentajes deben ser finitos, quedar entre el mínimo cerrado y 100, y satisfacer 90/85/85/80 global y 95/95/90/90 por scope para líneas, regiones, branches y mutation score. La v2 es incompatible con v1: amplía la frontera crítica y cambia el contexto de firma para impedir que evidencia anterior autorice un build bajo el contrato nuevo.

`--sign-release-readiness MANIFEST COVERAGE MUTATION FUZZ_CORPUS OUTPUT` vuelve a calcular los hashes de los tres artefactos, deserializa los dos reportes normalizados, compara exactamente build, scopes y métricas, exige el build integral de ese binario y firma con una subclave separada. La firma acredita la revisión del operador y la identidad de los bytes; no transforma cifras inventadas en evidencia válida ni demuestra por sí sola la duración declarada. El runtime rechaza schema, build, scope, umbral, firma o antigüedad incorrectos. La autorización v3 incorpora el SHA-256 del archivo readiness firmado.

## Replay

Cada línea puede ser un `MarketFrame` legado o un capture v1. El capture v1 envuelve el frame con `source=iol_v2_normalized`, `captured_at_secs`, schema y SHA-256 del JSON canónico del frame; modificar el frame sin actualizar el hash se rechaza. El lector valida orden temporal y rechaza symlinks, más de 1 GiB o más de 1.000.000 de frames. Ni ese hash por frame ni el SHA-256 del archivo demuestran procedencia externa o separación fuera de muestra. Por eso todo replay ordinario es `ResearchReplay` y no habilita `live`; el sellado requiere el manifiesto adicional descripto debajo.

La herramienta de registro de datasets sí emite y verifica manifiestos v1 firmados, pero no convierte por sí sola un replay en evidencia operativa. El manifiesto incluye `dataset_id=sha256:<hex>`, origen, licencia, intervalo, instrumentos, zona horaria, transformaciones, schema de origen, instante positivo de creación y particiones. El raw admite hasta 1 GiB, cada manifiesto hasta 256 KiB y el registro inspecciona como máximo 10.000 entradas. Los instrumentos únicos tienen 1–32 caracteres `A-Z`, `0-9` o `.`, y cada transformación declarada debe ser no vacía y única. Las particiones son intervalos cerrados —un único instante es válido— que no pueden solaparse. Registrar recalcula el hash del archivo y congela el split para ese dataset; el registro también rechaza cruces entre `selection` y `sealed_validation` del mismo instrumento. Consumir exige exactamente un holdout, `evaluator_id` de 1–128 caracteres sin controles e instante positivo antes de registrar, y escribe `holdout-v1-<hash_manifiesto>.json` con `create_new` antes de entregar el intervalo. Si la marca ya existe, sólo se reconoce como consumo previo cuando deserializa y coincide exactamente; una marca corrupta, sustituida o un error de escritura distinto de `AlreadyExists` bloquea como inconsistencia.

## Cotizaciones y órdenes IOL

Los precios deben ser positivos/finitos, libros no cruzados, timestamps ordenados y cotizaciones vigentes. El parser de orden no completa cantidades ni precios ausentes. Los fixtures de integración deben representar la respuesta contractual esperada antes de ejecutar el test; modificar el fixture para adaptarlo a la salida del código se considera una prueba inválida.

### Metadata contractual de opciones

El multiplicador se toma del catálogo `GET /api/v2/BCBA/Titulos/<ticker>/Opciones` para el símbolo exacto. Antes de normalizarlo, se calcula SHA-256 sobre el cuerpo HTTP exacto y se crea una copia inmutable en `DATA_DIR/catalog/<ticker>/iol-options-v1-<sha256>.json`; si el nombre ya existe, su contenido se vuelve a verificar y una divergencia bloquea la operación. Cada cotización conserva `catalog_contract_multiplier`, `catalog_observed_at_secs`, `catalog_schema_version=1`, `catalog_sha256`, `catalog_archived=true` y `contract_metadata_source=iol_catalog`. Una entrada real exige valor positivo, fuente IOL, observación no futura ni anterior a `CACHE_TTL_SECS`, schema conocido, hash no nulo y archivo confirmado; todos esos datos se congelan en `EntryContext`. El fallback global y una calibración histórica pueden ayudar a conciliar o cerrar exposición existente, pero no autorizan una compra real nueva.

Cada frame IOL registra además contratos de catálogo, filas de cotización, contratos aceptados —desglosados en CALL y PUT y por días al vencimiento—, faltantes e inválidos. Los conteos globales y por vencimiento deben reconciliar exactamente con la cadena conservada; TUI y log muestran la degradación en lugar de descartar contratos en silencio. Las nuevas entradas exigen el porcentaje y el mínimo por lado configurados dentro del tenor que la estrategia puede seleccionar; contratos fuera de ese rango no compensan faltantes operativos.

### Estado de cuenta y fondos

Antes de cada evaluación que podría generar una orden real, el motor consulta `GET /api/v2/estadocuenta`. El contrato se verificó el 25 de agosto de 2026 contra el Swagger oficial de IOL, cuyo SHA-256 era `710baa349625ed22a4d9ae125b2f4261d87459dd9ab6c1ec1cdec078dcd04f8b`.

Se exige exactamente una cuenta con `tipo=inversion_Argentina_Pesos`, `moneda=peso_Argentino` y `estado=operable`. `disponible` debe ser finito y no negativo; dentro de `saldos` debe existir exactamente un registro `liquidacion=inmediato` cuyo `disponibleOperar` también sea finito y no negativo. La capacidad efectiva de compra es el mínimo entre ambos importes y el presupuesto de estrategia ya reducido por etapa, VIX y mediodía.

Una cuenta ausente, duplicada, bloqueada, en otra moneda, sin liquidación inmediata o con importes inválidos bloquea la entrada. No se interpreta `saldo`, `total`, `margenDescubierto` ni títulos valorizados como efectivo disponible. Los IDs sólo se muestran enmascarados fuera de artefactos privados estrictamente necesarios.

La reconciliación de cartera y órdenes pendientes es total: cada fila debe ser objeto, tener símbolo y cantidad entera positiva dentro de rango. Una orden pendiente requiere además `broker_order_id` y lado compra/venta reconocible. Ninguna fila ambigua se descarta, se redondea o se completa con cero; invalida el snapshot completo y mantiene bloqueadas las entradas. Las fechas civiles del catálogo se validan como calendario real, por lo que un vencimiento imposible no se normaliza.

Fuente contractual: `https://api.invertironline.com/v2/swagger`. El hash identifica la versión auditada; un cambio del contrato obliga a revisar el fixture y el parser antes de promover `live`.
