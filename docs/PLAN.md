# Plan de mejora, coherencia y reducción de riesgo

## Estado y alcance

- **Fecha de la auditoría:** 25 de agosto de 2026.
- **Baseline revisada:** commit `99a3d9c` de la rama `develop`, más el traslado documental incluido en este cambio.
- **Alcance:** código Rust, configuración, persistencia, integración IOL, datos de mercado, VIX, calendario, aprendizaje, TUI, pruebas y toda la documentación versionada.
- **Naturaleza de este archivo:** plan de remediación. Una acción no debe considerarse implementada hasta que cumpla sus criterios de aceptación y esté respaldada por pruebas.

La conclusión actual es que `readonly` y replay pueden usarse para observación e investigación controlada. Los hallazgos P0 de implementación están corregidos o contenidos de forma fail-closed, pero **no se recomienda habilitar órdenes reales**: todavía faltan alcanzar los gates altos de cobertura/mutación, ejecutar campañas adversariales prolongadas, validar contra un sandbox oficial si existe y completar shadow/canary supervisados. Una respuesta ambigua continúa deteniendo el motor y exige conciliación; nunca se convierte en una ejecución supuesta.

## Avance de implementación

Estado de esta rama al 26 de agosto de 2026:

- **Implementado:** ORD-01–04, AUTH-01, NET-01, SEC-01, VIX-01, WS-01, DATA-01, PRC-01, RISK-01, la allowlist contractual de `IOL_ORDER_PATH` y el límite efectivo de `MAX_CONCURRENT_REQUESTS` de CFG-01.
- **Implementado:** el replay de órdenes liga cada actualización a una intención previa, revalida estado/cantidad/precio contra la solicitud original y rechaza cambios de `broker_order_id`, fills regresivos, terminales huérfanos y exposiciones parciales incoherentes. Sólo un terminal seguro de la misma intención libera el pendiente local.
- **Implementado:** el replay de posiciones mantiene una única exposición exacta y coherente entre motor y portfolio; es idempotente sólo ante el mismo contenido, rechaza aperturas/cierres contradictorios y no reanuda el motor si persiste un freno operativo. La campaña focalizada detectó 24/24 mutantes viables.
- **Implementado con contención conservadora:** CAL-01 exige manifiesto bursátil versionado y falla cerrado; EVI-01 incorpora un registro firmado e inmutable que verifica hash, procedencia, licencia, schema, instrumentos, zona, transformaciones y roles sin solapamiento; EVI-02 permite evaluar intervalos selection/holdout fijados por ese manifiesto y consume el holdout una sola vez antes de revelar métricas. El runner automático 80/20 sigue siendo diagnóstico y ningún replay se promueve automáticamente al gate de dinero real. VIX-02 mantiene el filtro inactivo cuando el feed no satisface vigencia.
- **Implementado:** FS-01/PST-01 tienen locks de kernel por raíz y modo, `0700`/`0600`, rechazo de symlink, temporales únicos, cuotas, validación sobre el descriptor abierto, secuencia contigua y cadena SHA-256. `live` escribe journal v6 autenticado con HMAC y rechaza clave incorrecta o una alteración cuyo atacante haya recalculado la cadena. El lock raíz impide carreras sobre recursos compartidos entre `readonly` y `live`. Pruebas reales cubren exclusión multiproceso, liberación tras matar el proceso, sobredimensión, truncamiento y todas las fronteras de escritura atómica. Un harness de proceso termina sin destructores después de intent durable, efecto broker controlado, aceptación, cancelación y terminal, y verifica exactamente qué estado sobrevive.
- **Parcial:** TEST-01 eliminó la URL sentinel y usa inyección explícita; existen tests HTTP de contrato, VIX, calendario, fills, anti-replay, render TUI, CLI y cortes de proceso del protocolo durable. `proptest` cubre propiedades independientes de P&L, dirección, estados/cantidades, sizing y riesgo; CI ejecuta fuzz smoke sobre JSONL replay, captures, identidad de journal, órdenes IOL y WebSocket desde semillas versionadas, impone pisos por módulo y la campaña semanal de mutación particiona todos los archivos Rust de `src/` en cinco dominios, además de calcular scores críticos. Las campañas focalizadas detectaron 154/154 mutantes viables de 158 generados en broker completo (cuatro no compilaban), 21/21 en replay de órdenes, 24/24 en replay de posiciones, 17/17 en el lifecycle IOL de órdenes, 31/31 en reconciliación IOL, 35/35 en fondos y multiplicador IOL, 46/46 en calibración de costos IOL, 29/29 en strike/tipo IOL, 47/47 en frame/catálogo/libro IOL, 18/18 en WebSocket IOL, 54/54 en autenticación/decodificación/retry IOL y 1/1 tras reestructurar el circuito, 87/87 en calidad/ejecución de cotizaciones de mercado, 42/42 en selección/moneyness y 91/91 en integridad de frames/calidad por vencimiento, 32/32 en riesgo, 19/19 en VIX, 11/11 en referencia horaria, 10/10 en identidad de build, 11/11 en redacción, 66/66 en readiness, 43/43 en filesystem seguro, 64/64 en persistencia, 90/90 en datasets, 90/90 en secretos, 165/165 en calendario y 228/228 en configuración. Un workflow semanal prolongado ejecuta una hora por target y conserva corpus/logs/build; falta obtener y revisar corridas verdes y alcanzar los gates altos previos a canary.
- **Implementado:** RISK-02 exige para cada entrada real multiplicador del símbolo, fuente `iol_catalog`, observación dentro del TTL, schema conocido, SHA-256 no nulo y archivo raw verificado. El cuerpo HTTP exacto se conserva de forma content-addressed y schema, hash, estado de archivo, instante, fuente y multiplicador se congelan en la posición.
- **Contrato externo verificado:** el Swagger oficial IOL auditado el 25 de agosto de 2026 confirma `POST /api/v2/operar` y `GET /api/v2/estadocuenta`. RISK-01 exige cuenta de inversión en pesos operable, saldo inmediato y limita cada entrada real con fondos recién consultados. El hash de la especificación y los campos usados están en `DATA_CONTRACTS.md`.
- **Implementado:** TIME-01 usa la zona IANA `America/Argentina/Buenos_Aires`, conserva hora de mercado y recepción, bloquea procedencia o desvíos inválidos y exige en `live` una referencia horaria HTTPS de origen distinto de IOL. Una referencia ausente, fallida o fuera del límite bloquea entradas nuevas, no salidas.
- **Implementado:** LOG-01 limita cuerpos, centraliza el saneamiento de texto libre antes de tracing/TUI/stderr, enmascara cuentas, correos e IDs externos y tiene una prueba estática de los sinks operativos. Los artefactos privados tipados conservan la correlación necesaria.
- **Implementado:** TEL-01 limita archivos individuales y el total de `DATA_DIR`, reserva espacio libre y poda únicamente captures vencidos. La telemetría no crítica se segmenta por día de rueda, el TUI muestra cuota y espacio libre y `telemetry/storage-pressure.json` expone una medición estructurada para supervisión externa. Journal, snapshots, órdenes, catálogos y evidencia quedan fuera de toda eliminación automática.
- **Implementado:** SUP-01 fija Rust 1.98.0/lockfile, ejecuta RustSec, dependency review, `cargo-deny` y Dependabot; TruffleHog queda fijado por commit e imagen y falla ante secretos verificados o desconocidos, y CI genera un CycloneDX 1.5 reproducible con `cargo-cyclonedx 0.5.9` y lo archiva 90 días.
- **Implementado como gate fail-closed:** Canary requiere readiness v2 firmado, vigente y ligado al hash de todo el código Rust, manifest, lockfile y toolchain, a los bytes exactos de los reportes normalizados de cobertura/mutación y del archivo de corpus fuzz, y a los umbrales 90/85/85/80 global y 95/95/90/90 críticos. La v2 incorpora además identidad de build, verificador de readiness, filesystem seguro y referencia horaria como scopes obligatorios; v1 se rechaza. La firma compara las métricas declaradas con los reportes; la autorización v3 liga el hash de ese readiness, Canary/Live lo revalidan en cada ciclo y una compra lo comprueba nuevamente antes del intent/POST. Las 128 combinaciones de siete gates tienen expected independiente; las 127 incompletas se ejecutan además como procesos reales contra un listener de contrato y terminan con cero conexiones al broker. La rama actual queda bloqueada porque todavía no alcanza los gates estadísticos ni operativos; los tests sintéticos sólo demuestran sus contratos.
- **Contención vigente:** el registro y la evaluación sellada ya existen, pero la importación automática de evidencia histórica al gate permanece deliberadamente deshabilitada. La promoción exige una revisión humana explícita del manifiesto, el resultado sellado y los gates cuantitativos pendientes.

La clasificación anterior es deliberadamente estricta: una mitigación fail-closed no se presenta como validación positiva de un proveedor externo.

## Resultado ejecutivo de la baseline

La baseline original presentaba estas divergencias materiales, hoy contenidas o corregidas en esta rama:

1. una orden informada como `Ejecutada` sin cantidad puede convertirse localmente en un fill completo inventado;
2. el evento previo al envío se hace `flush`, pero no queda necesariamente durable antes del `POST` real;
3. un `POST` de orden puede repetirse automáticamente después de un `401` sin demostrar que el primer intento no tuvo efecto;
4. el calendario de feriados nacionales no representa por sí solo el calendario ni los horarios especiales de negociación de BYMA;
5. cualquier replay se etiqueta automáticamente como evidencia histórica fuera de muestra, aunque no haya prueba de procedencia, separación temporal ni ausencia de ajuste previo;
6. la autorización de `live` es un JSON no autenticado y la protección de la contraseña deriva su clave de un identificador público de la máquina;
7. la cobertura total era moderada y dos superficies operativas importantes, `vix` y `main`, tenían 0 % de líneas cubiertas; la medición posterior se mantiene separada más abajo.

La contención y las correcciones de ejecución ya están aplicadas. Aun así, no debe considerarse un canary hasta completar los criterios pendientes de pruebas adversariales, evidencia sellada y operación supervisada.

## Método y baseline verificable

La revisión comparó `README.md`, `.env.example`, las instrucciones del repositorio, todos los documentos de `docs/` y los módulos Rust. No se enviaron órdenes ni se consultó una cuenta real.

Como contención documental ya aplicada en esta auditoría, `ALGORITMO.md` se trasladó a `docs/`, se corrigieron sus enlaces y versiones de schema, se actualizaron las referencias principales y se marcó como histórica la documentación todavía no consolidada.

Se ejecutó:

```text
cargo llvm-cov --all-targets --locked --summary-only
```

Resultado de la baseline original (conservado para trazabilidad; no describe la rama actual):

| Métrica | Baseline |
|---|---:|
| Tests | 148 aprobados, 0 fallidos |
| Líneas | 71,84 % |
| Regiones | 67,57 % |
| Funciones | 71,88 % |
| Branches | no instrumentadas/informadas |

Módulos que requieren atención prioritaria:

| Módulo | Cobertura de líneas | Observación |
|---|---:|---|
| `src/vix.rs` | 0,00 % | Contrato HTTP y frescura sin prueba directa |
| `src/main.rs` | 0,00 % | CLI, autorización y shutdown sin prueba directa |
| `src/tui.rs` | 15,27 % | Estados operativos y tamaños de terminal poco cubiertos |
| `src/config.rs` | 62,23 % | Gran superficie de combinaciones y validaciones |
| `src/app.rs` | 69,58 % | Orquestación, órdenes y recuperación concentran riesgo |
| `src/iol_client.rs` | 75,35 % | Buen comienzo, insuficiente para protocolo de órdenes |

La rama actual ya contiene suites separadas en `tests/`, inyección explícita de fuentes y pruebas de CLI como proceso. El sentinel `cfg!(test)` + URL mágica fue eliminado; este párrafo queda como registro del hallazgo original.

## Fuente de verdad documental

El destino propuesto es:

| Documento | Rol definitivo | Acción |
|---|---|---|
| `docs/ALGORITMO.md` | Contrato funcional, precedencias y defaults | **Consolidado**; corregir sólo junto con código o decisión explícita |
| `README.md` | Instalación, operación básica y límites | **Consolidado** y enlazado al contrato canónico |
| `docs/ARCHITECTURE.md` | Arquitectura efectivamente implementada | **Reescrito** desde módulos, flujos y fronteras reales |
| `docs/IMPLEMENTATION_DETAILS.md` | Contratos técnicos comprobables | **Reescrito** sin pseudocódigo operativo ni rutas inexistentes |
| `docs/DEPLOYMENT.md` | Runbook probado | **Reescrito** sin base de datos ni capacidades ficticias |
| `docs/EXECUTIVE_SUMMARY.md` | Estado y riesgos medidos | **Corregido** sin promesas de producción, RTO o rentabilidad |
| `docs/INDEX.md` | Navegación | **Simplificado** y cubierto por link checker |
| `.github/copilot-instructions.md` | Reglas para cambios futuros | **Alineado** con documentos canónicos e invariantes reales |

Se verificó que los nombres de variables de configuración ejecutables están representados en `docs/ALGORITMO.md`. Las principales divergencias restantes están en valores derivados, efecto real de algunos flags y afirmaciones operativas.

### Divergencias documentales confirmadas

- El `ALGORITMO.md` de origen declaraba snapshot v3, journal v2 y evidencia v5. El contrato vigente declara snapshot v4, journal v6, evidencia v6 y analytics v2; `tests/documentation_contract.rs` impide automáticamente que esas constantes centrales o los nombres ejecutables de `.env.example` vuelvan a divergir.
- `docs/ARCHITECTURE.md` presenta estructura propuesta, persistencia opcional, hot reload, health checks y un shutdown que cerraría posiciones; esas capacidades no describen el runtime actual.
- `docs/IMPLEMENTATION_DETAILS.md` incluye una orden de mercado como último recurso y afirma que un spread amplio dispara el kill switch. El algoritmo actual prohíbe market orders y un spread amplio bloquea una entrada, no la gestión de salida.
- `docs/DEPLOYMENT.md` documenta una base de datos, pruebas y health checks inexistentes; algunos comandos toleran fallos con `|| true`, y afirma que `Ctrl+C` cierra posiciones. El código preserva la posición y marca el shutdown como no limpio.
- `docs/EXECUTIVE_SUMMARY.md` mezcla objetivos con resultados no medidos y describe snapshots comprimidos y defaults desactualizados.
- `docs/INDEX.md` contiene conteos de líneas y descripciones que envejecieron y presenta documentos históricos como si fueran contractuales.
- `README.md` dice que el holdout final permanece intacto. El split 80/20 se recalcula y vuelve a evaluar al incorporar operaciones; por lo tanto no es un holdout final sellado.

## Registro priorizado de riesgos y correcciones

**P0** bloquea `live`. **P1** debe cerrarse antes de promover evidencia o ampliar un canary. **P2** mejora mantenibilidad, observabilidad u operación.

| ID | Prioridad | Hallazgo y evidencia | Destino |
|---|---|---|---|
| ORD-01 | P0 | **Corregido:** una cantidad ausente nunca se completa desde la solicitud; una ejecución terminal exige cantidad y precio explícitos. | Código + contrato documental |
| ORD-02 | P0 | **Corregido:** la intención se sincroniza antes del efecto externo y un harness mata el proceso en cada frontera durable. | Código + pruebas de crash |
| ORD-03 | P0 | **Corregido:** un `401` posterior al envío renueva credenciales pero no repite el `POST`; la orden queda para conciliación. | Código + contrato IOL |
| ORD-04 | P0 | **Corregido:** matriz cerrada de estado, cantidad, precio e ID, con transiciones y parciales conservadores. | Código + UT por tabla |
| CAL-01 | P0 | **Corregido con contención:** `live` exige un manifiesto bursátil versionado que modela cierres y horarios especiales; el feriado civil es auxiliar. | Código + datos + docs |
| EVI-01 | P0 | **Corregido con contención:** replay ordinario es `ResearchReplay`; el registro firmado verifica identidad, procedencia y particiones, pero la promoción histórica automática continúa deshabilitada. | Código + gobierno de datos |
| AUTH-01 | P0 | **Corregido:** grant HMAC efímero, ligado a cuenta/fingerprint/límites, nonce no reutilizable y clave externa. | Código + modelo de amenazas |
| NET-01 | P0 | **Corregido:** HTTPS/WSS, redirects deshabilitados, límites de cuerpo/frame y loopback HTTP sólo para adaptadores expresamente admitidos. | Código + configuración |
| SEC-01 | P0 | **Corregido:** `live` exige clave maestra aleatoria externa y credencial `v3:` con subclave HKDF-SHA256 versionada; `v2:` y machine-id quedan sólo para migración readonly. | Código + migración operativa |
| FS-01 | P0 | **Corregido:** permisos privados, rechazo de symlinks, temporales únicos, locks de raíz/modo, cuota agregada y pruebas multiproceso/fault injection. | Código + pruebas concurrentes |
| EVI-02 | P1 | **Corregido para evaluación sellada:** `run_sealed_temporal_experiment` usa los intervalos firmados y crea una marca durable de consumo antes de calcular métricas. El runner automático 80/20 conserva nombre y estado diagnóstico, sin autoridad operativa. | Código + metodología |
| VIX-01 | P1 | **Corregido:** nivel y cierre previo tienen vigencias independientes; un cierre vencido no calcula variación. | Código + contrato de datos |
| VIX-02 | P1 | **Contenido:** un feed sin nivel `current` dentro de vigencia se muestra no vigente y el ajuste queda inactivo. | Datos + configuración + docs |
| WS-01 | P1 | **Corregido:** desactivado por default, WSS, límites y cola acotada; sólo adelanta consultas REST autoritativas. | Código + TUI + docs |
| CFG-01 | P1 | **Corregido:** semáforo HTTP efectivo; límite dinámico queda identificado explícitamente como investigación sin ruta real. | Código + docs |
| CFG-02 | P1 | **Contenido:** spreads verticales permanecen `shadow_only`; la variable no habilita una ruta secuencial ni afirma soporte atómico. | Código + docs |
| DATA-01 | P1 | **Corregido:** límites previos de bytes/filas/frames y métricas reconciliables de contratos aceptados, faltantes e inválidos. | Código + observabilidad |
| DATA-02 | P1 | **Corregido:** ticker con alfabeto acotado y ruta de alta de orden bajo allowlist contractual exacta. | Código + configuración |
| PRC-01 | P1 | **Corregido:** entradas incompatibles producen error explícito y casos publicados/oracle independiente validan el pricer. | Código + oracle independiente |
| RISK-01 | P1 | **Corregido:** `estadocuenta` exige cuenta de inversión ARS operable y saldo inmediato; cada posible entrada real vuelve a consultar y limita el presupuesto al menor fondo disponible. | Código + contrato de cuenta + fixtures independientes |
| RISK-02 | P1 | **Corregido:** la entrada real exige metadata del catálogo exacto por símbolo, vigente, versionada, archivada por SHA-256 y congelada en la posición; no usa el fallback global. | Código + datos maestros |
| TIME-01 | P1 | **Corregido:** zona IANA y sesión, timestamps exchange/receive y una referencia HTTPS independiente de IOL con límites cerrados. Sus fallos bloquean entradas nuevas sin impedir salidas. | Código + pruebas temporales |
| PST-01 | P1 | **Corregido:** tamaños/eventos acotados, secuencia continua, cadena de hash y HMAC obligatorio para journal `live`. | Código + recuperación |
| OPS-01 | P1 | **Corregido documentalmente:** shutdown preserva y sincroniza estado; no promete cerrar exposición sin una política aprobada y confirmación de mercado. | Docs + runbook |
| LOG-01 | P1 | **Corregido:** saneador central acotado en todos los sinks de texto libre, cuerpos de autenticación omitidos, IDs/cuentas/correos enmascarados y auditoría automática del contrato de sinks. | Código + política de datos |
| TEST-01 | P1 | **Parcial:** suite de protocolo/proceso, properties, fuzz smoke, mutación crítica y pisos CI vigentes; faltan campañas largas y gates pre-canary. | Arquitectura de tests + CI |
| SUP-01 | P2 | **Corregido:** toolchain/lockfile, RustSec, dependency review, cargo-deny, Dependabot, secret scanning fijado y SBOM reproducible. | CI + docs |
| TEL-01 | P2 | **Corregido:** cuota agregada, reserva, archivos acotados, segmentación diaria de telemetría, retención exclusiva de captures y snapshot estructurado de presión; el margen insuficiente detiene el motor antes de escribir. | Código + operación |

## Fase 0 — Contención y verdad operativa

**Estado: cerrada localmente el 26 de agosto de 2026.** El corte solicitado se realiza al completar esta fase. Las implementaciones de fases posteriores que ya existen en la rama se conservan, pero no se consideran promovidas ni sustituyen sus criterios de aceptación pendientes.

### 0.1 Mantener deshabilitada la ruta real

- Mantener `MODE=readonly` y no emitir nuevas autorizaciones mientras exista un P0 abierto.
- **Implementado:** la comprobación de readiness lista bloqueos de contrato/configuración y hace imposible armar Canary si faltan transporte, calendario, reloj, persistencia, firma o evidencia de calidad ligada al build. Además, `MODE=live` rechaza al inicio cualquiera de las 127 combinaciones incompletas de siete gates.
- Separar “servicio IOL conectado”, “WebSocket”, “mercado” y “capacidad de operar” en estados distintos. `ONLINE` nunca debe implicar que el WebSocket funciona ni que `live` está autorizado.

**Aceptación cumplida localmente:** una prueba de proceso intenta cada combinación incompleta; todas terminan durante validación y el listener del broker observa cero conexiones, por lo que ningún `POST` puede emitirse.

### 0.2 Corregir documentación factual inmediata

- Mantener la corrección ya aplicada de las cuatro versiones de schema y agregar una tabla de compatibilidad/migración verificada automáticamente.
- Corregir shutdown, persistencia, defaults y estado real de límites dinámicos/verticales.
- Retirar toda mención a market orders, cierre automático al recibir señal y capacidades inexistentes.
- Marcar métricas de rendimiento, RTO, latencia y cobertura como objetivos hasta medirlas en CI/operación.

**Aceptación cumplida localmente:** `tests/documentation_contract.rs` compara constantes de schema y compatibilidad, verifica todos los links locales y contrasta los defaults ejecutables documentados con `.env.example`; los tests de configuración fijan sus valores y restricciones.

## Fase 1 — Órdenes, idempotencia y recuperación

### 1.1 Contrato estricto de ejecución

Crear un resultado tipado que distinga `Pending`, `PartiallyExecuted`, `Executed`, `Rejected`, `Cancelled`, `Unknown` e `Inconsistent`. No completar campos ausentes con la solicitud original.

Validaciones mínimas:

- `Executed` exige `broker_order_id`, precio finito positivo y cantidad exactamente solicitada;
- `PartiallyExecuted` exige `0 < filled < requested` y precio del fill;
- `Rejected`/`Cancelled` no pueden aumentar cantidad ejecutada sin entrar en una variante explícita que preserve el parcial;
- cantidad negativa, fraccionaria, mayor a la solicitada o ausente es inconsistente;
- estado desconocido permanece `Unknown`; nunca se traduce silenciosamente a `Pending` si eso pierde información;
- un cambio regresivo de estado se rechaza y activa reconciliación.

**Aceptación:** matriz exhaustiva de estados × cantidad × precio × ID, con fixtures JSON y resultado esperado especificado antes de ejecutar el parser.

### 1.2 Protocolo durable de orden

Implementar un state machine persistente:

```text
IntentCreated -> IntentDurable -> SubmittedUnknown/Accepted ->
Pending/PartiallyFilled -> Executed/Rejected/Cancelled/UnknownTerminal
```

- Hacer `sync_data`/`sync_all` del intent antes del primer `POST` y sincronizar el directorio cuando corresponda.
- Persistir `broker_order_id` en cuanto aparece.
- Definir un `client_operation_id` único, estable entre reinicios y aceptado por el contrato del broker.
- Ante timeout, corte, `401` o respuesta malformada, consultar por ese identificador antes de retransmitir.
- Si IOL no garantiza idempotencia o consulta correlacionable, no retransmitir automáticamente: bloquear y reconciliar.
- Después del timeout, cancelar sólo una orden identificada; seguir consultando hasta terminal o `UnknownTerminal`.

**Aceptación:** pruebas de proceso terminan abruptamente la aplicación en cada frontera anterior y posterior a escritura, `fsync`, `POST`, respuesta, cancelación y snapshot. Al reiniciar, la suma de fills broker/local coincide y nunca se observa una orden duplicada.

### 1.3 Persistencia íntegra

- Reemplazar el PID-file recursivo por un lock del kernel mantenido por descriptor; guardar metadata sólo para diagnóstico.
- Aplicar `0700` a directorios sensibles y `0600` a archivos desde el momento de creación, sin ventana previa al `chmod`.
- Usar temporales únicos, `O_NOFOLLOW`, confinamiento al `DATA_DIR` canónico y `rename` + sync de directorio.
- Proteger recursos compartidos entre `readonly` y `live` con locks del recurso, no sólo del modo.
- Añadir secuencia contigua, hash encadenado/HMAC, detección de truncamiento y recuperación explícita de última línea rota.
- Leer por streaming con límites de bytes, registros y antigüedad; cuarentenar corrupción en vez de ignorarla.

**Aceptación:** tests con dos procesos, symlink hostil, permisos incorrectos, archivo truncado, secuencia repetida/faltante, escritura parcial, falta de espacio y kill en rename/sync.

## Fase 2 — Fronteras de confianza y seguridad

### 2.1 Transporte y contratos HTTP/WS

- Parsear URLs; exigir HTTPS/WSS y hosts/puertos permitidos en `live`.
- Permitir HTTP sólo en loopback bajo un modo de prueba explícito que no pueda enviar órdenes.
- Deshabilitar redirects con credenciales o, como mínimo, impedir cambios de origen.
- Aplicar timeout de conexión/respuesta, límite de body, `Content-Type`, esquema y cardinalidad a IOL, VIX y calendario.
- Validar ticker, endpoint y método contra el contrato autorizado; no aceptar rutas arbitrarias en `live`.
- Redactar y limitar cuerpos de error antes de logs.

**Aceptación:** servidor de contrato prueba TLS inválido, downgrade, redirect cross-origin, 401/429/5xx, body excesivo, content type erróneo, JSON truncado y respuesta lenta sin filtrar secretos.

### 2.2 Secretos y autorización

- Migrar contraseña/token a keyring del SO o secret manager. Como fallback operativo, usar una clave aleatoria root/operator-owned, separada de `.env`, con HKDF, rotación y versión; no derivarla de `machine-id`.
- Retirar el password de argumentos CLI (`-e`) porque queda expuesto en historial y lista de procesos; usar prompt sin eco o entrada segura documentada.
- Firmar la autorización con una clave de operador externa al `DATA_DIR` o validarla mediante keyring/HSM. Incluir nonce, cuenta, epoch, fingerprint, hash del build, límites y expiración.
- Consumirla atómicamente con protección contra replay, symlink y sustitución concurrente.

**Aceptación:** modificar cualquier byte, copiar una autorización vieja, cambiar reloj, cuenta, binary hash o límite hace fallar cerrado; no aparecen secretos en argv, logs ni artefactos CI.

### 2.3 WebSocket opcional y acotado

- Incorporar `IOL_WEBSOCKET_ENABLED=false` por default mientras el contrato correlacionable no esté validado.
- Mantener REST como autoridad de estado. El WS sólo adelanta una consulta si contiene un ID verificable; jamás confirma un fill por similitud de símbolo/cantidad.
- Cambiar canales sin límite por colas acotadas con coalescing/contador de descartes, máximo de frame/mensaje, timeout de conexión, ping y backoff con jitter.
- TUI: `WS DESACTIVADO`, `WS CONECTADO`, `WS RECONECTANDO` o `WS OFFLINE`. El detalle técnico queda sólo en el historial/log.

**Aceptación:** una ráfaga superior a la capacidad no aumenta memoria sin cota, no bloquea el loop de riesgo y deja métrica de descartes; deshabilitado significa cero intentos de conexión.

## Fase 3 — Tiempo, calendario y datos de mercado

### 3.1 Calendario de rueda

Modelar sesiones del mercado, no sólo feriados civiles:

- fuente primaria: calendario/boletín de BYMA por instrumento o segmento;
- estados: `Open`, `Closed`, `SpecialHours`, `TradingWithoutSettlement`, `Unknown`;
- ArgentinaDatos puede ser una señal auxiliar, no autoridad suficiente para negociación;
- cache versionado con fuente, fecha de emisión, fecha de consulta, hash y vencimiento;
- override de emergencia firmado, temporal y auditable;
- ante ausencia o contradicción, fallar cerrado para entradas reales.

El caso de prueba debe incluir días que distingan feriado nacional de rueda bursátil y el cierre/sesión especial publicado por BYMA; no se debe construir la expectativa a partir de lo que hoy devuelve `MarketCalendar`.

**Aceptación:** tabla anual de sesiones derivada de fuentes archivadas, revisada independientemente, con pruebas para apertura 10:30, observación inicial, cierre 17:00, fin de semana, vísperas, vencimiento, ruedas especiales, cambio de año y fuente caída.

### 3.2 Reloj y timestamps

- Usar `America/Argentina/Buenos_Aires` y un identificador de sesión en lugar de restar tres horas manualmente.
- Usar tiempo monotónico para timeouts y tiempo UTC con fuente para eventos.
- Medir desvío contra broker/fuente y detener nuevas entradas si excede el límite.
- Definir explícitamente exchange time, source time, receive time e ingest time.

### 3.3 VIX vigente y con procedencia

- Validar por separado timestamp del nivel y timestamp del cierre previo.
- Agregar proveedor, instrumento exacto, clase de demora, licencia y hora de recepción al contrato.
- No scrapear ni presentar como vigente un valor público demorado. Si no existe una fuente autorizada dentro de `VIX_MAX_AGE_SECS`, mostrar `VIX NO VIGENTE` y no aplicarlo al algoritmo.
- Definir qué sesión constituye “previous close” y rechazar cierres de una sesión incompatible.

**Aceptación:** tests de caché, stale/futuro, reloj desviado, cierre previo ausente/antiguo, `NaN`/infinito, token, timeout, content type y límite de body. Un feed con demora superior al máximo nunca activa el ajuste VIX.

### 3.4 Calidad de cadena de opciones

- Informar cantidad recibida, aceptada y rechazada por razón.
- Exigir completitud mínima configurable por lado/tenor y detener entradas ante degradación.
- Versionar captures JSONL con schema, fuente, hash, timestamps, secuencia y metadata de instrumento.
- Obtener multiplicador, estilo, vencimiento y moneda de metadata punto-en-tiempo; una corrección manual requiere autorización auditable.

## Fase 4 — Validez del algoritmo y del aprendizaje

### 4.1 Separar investigación de autorización operativa

Reemplazar la inferencia “archivo replay = fuera de muestra” por un registro de datasets inmutable:

- ID, hash, origen, licencia, intervalo, instrumentos, zona horaria, transformaciones y versión de schema;
- roles incompatibles: `research`, `selection`, `sealed_validation`, `shadow`, `canary`, `live`;
- split cronológico definido y firmado antes del tuning;
- detección de solapamiento, duplicados, revisiones posteriores y leakage;
- un replay arbitrario o sintético nunca aporta evidencia para habilitar `live`.

**Aceptación:** intentar reutilizar sesiones entre selection y holdout, cambiar un byte, regenerar el split o importar un manifest no firmado invalida el gate.

### 4.2 Holdout realmente sellado

- Registrar una sola vez el inicio/fin del holdout y su hash.
- Prohibir recalcularlo al sumar operaciones o mirar resultados parciales.
- Si se consulta para tomar decisiones, marcarlo consumido y exigir un nuevo dataset futuro para confirmación.
- Aplicar walk-forward purgado con embargo, tuning anidado y controles por múltiples hipótesis.
- Calcular intervalos por sesión, no asumir trades independientes; reportar incertidumbre y sensibilidad a costos/liquidez.

### 4.3 Hipótesis de horario y volatilidad

Tratar apertura 45 minutos, lunch slowdown, weekend risk, VIX, umbrales de tendencia, IV Rank y meta-filtros como hipótesis sujetas a validación temporal, no como alpha demostrado.

- Comparar política base y cada cambio sobre las mismas sesiones elegibles.
- Incluir no-fill, fills parciales, latencia, spread, profundidad, selección adversa, costos e impuestos.
- Reportar cobertura: una estrategia que evita casi todas las observaciones no puede aprobar sólo por buen promedio.
- Medir drift por régimen y degradación fuera de muestra.
- Promover un cambio por vez: shadow, holdout sellado, canary acotado y revisión humana.

### 4.4 Riesgo de cuenta y posición

- **Implementado para la estrategia long-only actual:** consultar antes de cada posible entrada una cuenta ARS operable, `disponible` y `disponibleOperar` inmediato; dimensionar con el menor. No se usa margen para comprar opciones largas. Una futura pata corta deberá añadir y validar margen antes de salir de `shadow_only`.
- **Decisión implementada:** el límite diario acumula P&L realizado neto. No incorpora mark-to-market; la única posición larga abierta se limita por pérdida máxima congelada, stop, vencimiento, liquidez y reglas de gap/weekend. Documentar explícitamente que `MAX_DAILY_LOSS` no representa exposición intradía no realizada.
- Verificar pérdida máxima por instrumento, concentración, liquidez para salida y gap/weekend.
- No cerrar forzosamente con market order. Si no hay bid válido o el estado es incierto, conservar la exposición local, bloquear nuevas órdenes y alertar.

### 4.5 Analítica cuantitativa

- Cambiar el pricer a `Result` tipado para parámetros inválidos y probabilidad de transición fuera de rango.
- Validar precio, IV y Greeks contra un oracle independiente versionado, por ejemplo QuantLib, con fixtures golden.
- Añadir invariantes: límites de arbitraje, monotonicidad, put/call apropiado, convergencia por pasos y sensibilidad a tasa/dividendo/volatilidad.
- Mantener TWAP, límites dinámicos y verticales sólo en investigación hasta demostrar contrato, atomicidad, datos de profundidad y recuperación completa.

## Estrategia de pruebas de alta cobertura y sin trampas

### Principio de los datos de prueba

Cada prueba debe seguir esta dirección:

```text
fuente o especificación independiente -> entrada inmutable -> expectativa revisada -> función bajo prueba
```

Queda prohibido generar la expectativa llamando al mismo código, ajustar un fixture después de ver la salida para que pase, usar URLs mágicas, relajar tolerancias sin justificación, ignorar fallos con `|| true`, reintentar tests flaky hasta que pasen o contar datos sintéticos como evidencia de rentabilidad.

Los datos sintéticos sí son válidos para probar una propiedad definida de antemano —por ejemplo, “una cantidad negativa se rechaza” o monotonicidad del precio—, pero deben etiquetarse como sintéticos y no pueden habilitar Learning/Canary/Live.

### Procedencia y oracle

Cada fixture contractual debe incluir:

- fuente, fecha de captura y versión del contrato;
- hash del raw original y descripción de cualquier anonimización;
- campos esperados curados separadamente;
- zona horaria, unidad, moneda y semántica de timestamps;
- licencia/permiso de conservación;
- casos positivos, negativos, límites y controles que deban fallar.

Ejemplos:

- IOL: respuestas oficiales o capturas sanitizadas que preserven estructura; el expected describe hechos semánticos y no copia la salida del parser.
- Calendario: raw publicado por BYMA/ArgentinaDatos más una tabla de sesiones curada por revisión humana.
- Pricer: valores generados fuera del crate con una versión fijada de QuantLib; tolerancia basada en error numérico documentado.
- Replay: dataset raw read-only, manifest firmado, split cronológico predefinido y prueba automática de no solapamiento.

### Unit tests

- Tablas exhaustivas para parser y state machine de órdenes.
- Boundaries exactos de todos los límites de riesgo, horarios, antigüedad y configuración.
- Property tests con `proptest` para P&L, sizing, serialización, monotonicidad, idempotencia y timestamps.
- Model-based testing del lifecycle de una orden y su reconciliación.
- Tests de migración por cada versión soportada; schema desconocido debe fallar cerrado.
- Test del pricer contra oracle y propiedades financieras independientes.
- Tests de redacción que demuestren que tokens, passwords e IDs sensibles no aparecen.

### Integration tests

Crear una suite en `tests/` que use los clientes reales de `reqwest`/WebSocket contra servidores de contrato controlados:

1. autenticación, refresh, 401, 429, 5xx, timeout, desconexión y redirects;
2. submit, consulta, parcial, cancelación, terminal y resultado desconocido por `broker_order_id`/client ID;
3. frames WS válidos, fragmentados, malformados, excesivos y en ráfaga;
4. VIX y calendario con respuestas raw versionadas, cache y caída de fuente;
5. dos instancias concurrentes y recursos compartidos;
6. permisos, symlinks, truncamiento, disco lleno y restart después de kill;
7. render de TUI para cada estado y varios tamaños de terminal;
8. proceso completo de replay sobre dataset inmutable, comparando decisiones con un oracle de eventos predefinido.

El servidor de contrato no es una trampa: su state machine y respuestas se especifican independientemente y la aplicación se conecta mediante el mismo stack de red que usaría en producción. No debe existir un branch `cfg(test)` que cambie el significado de una orden.

No se enviarán órdenes reales desde CI. Si IOL ofrece sandbox/certificación, se agregará una etapa separada, sin credenciales en forks, con cuenta sin fondos reales, autorización manual y limpieza/reconciliación obligatoria.

### Fault injection, fuzzing y mutación

- Ejecutar subprocess tests y matar el proceso en cada frontera durable de órdenes/persistencia.
- Fuzzear parsers JSON, journal, captures y frames WS con `cargo-fuzz`.
- Usar `cargo-mutants`; una prueba que sigue verde al eliminar una validación crítica no es suficiente.
- Añadir tests de concurrencia determinista donde haya estado compartido.
- Medir recursos bajo bodies grandes, colas WS y replay extenso.

### Objetivos y gates de cobertura

La cobertura es un indicador, no el objetivo único. Se propone avanzar en dos gates:

La medición reproducible posterior a las correcciones usa un directorio de instrumentación estable aislado del job nocturno: 88,88 % de líneas, 87,53 % de regiones y 88,98 % de funciones. La verificación completa ejecuta 432 tests (402 de librería, tres de binario y veintisiete de integración), todos aprobados. App quedó en 78,09 %, broker en 99,72 %, autorización/CLI en `main` en 86,89 %, configuración en 99,71 %, calendario en 99,21 %, datasets en 99,11 %, IOL en 93,30 %, mercado en 94,79 %, persistencia en 97,31 %, secretos en 99,79 %, filesystem seguro en 99,45 %, VIX en 100 %, referencia horaria/riesgo en 100 %, readiness en 99,16 % y redacción/identidad de build en 100 % de líneas. CI mantiene un piso global de 80 % y pisos de líneas/regiones por módulo crítico para impedir regresiones. Los pisos se elevaron detrás de la evidencia: app 78/75, broker 99/99, main 86/81, configuración 98/97, calendario 99/98, datasets 99/95, IOL 86/84, mercado 94/95, persistencia 97/96, readiness 99/98, riesgo 100/100, secretos 99/95, filesystem seguro 99/95 y referencia horaria 100/97.

La última medición nocturna aislada con branches reales —anterior a los tests de autenticación, retry y mercado aquí informados— dio 87,47 % de líneas, 86,17 % de regiones y 66,74 % de branches globales. Ningún scope satisface todavía el gate crítico completo porque falta además una campaña de mutación global aceptable: `app::order_recovery` tiene ahora 78,09/75,55 de líneas/regiones y conserva el branch score previo de 50,00; broker mide ahora 99,72/99,57 de líneas/regiones y detectó 154/154 mutantes viables de 158 generados; autorización/CLI en `main` queda en 86,89/81,47/74,32 e IOL tiene ahora 93,30/92,21 de líneas/regiones, con un branch score previo de 68,53. Configuración queda en 99,71/98,70/95,40 y detectó 228/228 mutantes viables de 233 generados; los otros cinco no compilaban. Persistencia alcanzó 97,31/96,05/92,31. `market.rs` subió a 94,79/95,44 de líneas/regiones; el agregado `data_contracts` queda limitado en stable por IOL a 93,30/92,21, mientras su branch score previo permanece limitado por mercado en 60,80. Calendario queda en 99,21/98,85/96,39, datasets en 99,11/95,56/96,25 y secretos en 99,79/95,68/95,00. En cobertura ya superan 95/95/90 broker, configuración, calendario, datasets, identidad de build, persistencia, readiness, riesgo, secretos, filesystem seguro, referencia horaria y VIX. Las campañas focalizadas detectaron 21/21 mutantes viables en replay de órdenes, 24/24 en replay de posiciones, 17/17 en el lifecycle IOL de órdenes, 31/31 en reconciliación IOL, 35/35 en fondos y multiplicador IOL, 46/46 en calibración de costos IOL, 29/29 en strike/tipo IOL, 47/47 en frame/catálogo/libro IOL, 18/18 en WebSocket IOL, 54/54 en autenticación/decodificación/retry IOL y 1/1 tras reestructurar el circuito, 87/87 en calidad/ejecución de cotizaciones de mercado, 42/42 en selección/moneyness y 91/91 en integridad de frames/calidad por vencimiento, 32/32 en riesgo, 19/19 en VIX, 11/11 en referencia horaria, 10/10 en identidad de build, 11/11 en redacción, 66/66 en readiness, 43/43 en filesystem seguro, 64/64 en persistencia, 90/90 en datasets, 90/90 en secretos, 165/165 en calendario y 228/228 en configuración; no sustituyen el score de los módulos aún no barridos ni la campaña global. Los valores globales mantienen Canary bloqueado; deben elevarse mediante tests semánticos de orquestación, protocolo y fallas de proceso, no mediante exclusiones o acumulación de perfiles.

| Gate | Líneas | Regiones | Branch/condición | Mutation score |
|---|---:|---:|---:|---:|
| Toda PR | ≥ 95 % de líneas cambiadas y sin caída global | sin caída | habilitado en CI | mutantes del área crítica |
| Antes de canary | ≥ 90 % global | ≥ 85 % global | ≥ 85 % global | ≥ 80 % global |
| Módulos críticos antes de `live` | ≥ 95 % | ≥ 95 % | ≥ 90 % | ≥ 90 % |

Scopes críticos: `app::order_recovery`, `build_identity`, `config`, `data_contracts`, `iol_client`, `main::authorization`, `market_calendar`, `persistence`, `release_readiness`, `risk`, `secrets`, `secure_fs`, `time_reference` y `vix`.

Sólo se excluirá código generado o una rama demostrablemente no ejecutable, con justificación revisada. No se aceptan exclusiones para mejorar el porcentaje, ni asserts sin semántica, ni tests que sólo comprueben que “no hizo panic”.

### Pipeline CI propuesto

1. `cargo fmt --check` y `cargo clippy --all-targets --all-features -- -D warnings`;
2. UT y doctests con toolchain/lockfile fijados;
3. integración de protocolo/proceso;
4. `cargo llvm-cov` estable con pisos anti-regresión y un job nightly aislado que normaliza líneas/regiones/branches por scope para el gate pre-canary;
5. fuzz smoke por PR y campaña extendida programada;
6. mutación programada sobre cinco dominios que cubren todo `src/`, normalizada desde `caught`/`missed`/`timeout`, con agregado global y scores críticos obligatorios antes de firmar readiness;
7. auditoría de vulnerabilidades/licencias, secret scanning y SBOM;
8. archivo de reportes, fixtures/manifests y resultados reproducibles.

## Consolidación documental aplicada y pendiente

Ya se aplicó en esta rama:

- `docs/ALGORITMO.md` es el contrato consolidado de conducta, precedencias, schemas y defaults;
- `DATA_CONTRACTS.md`, `SECURITY.md` y `TESTING.md` separan procedencia, amenazas y pruebas;
- `ARCHITECTURE.md`, `IMPLEMENTATION_DETAILS.md`, `DEPLOYMENT.md`, `EXECUTIVE_SUMMARY.md` e `INDEX.md` describen el runtime real sin base de datos, market orders ni capacidades ficticias;
- el checker automático valida variables, versiones centrales y links locales.

Queda pendiente generar los valores de defaults directamente desde un schema tipado —hoy el checker garantiza nombres, mientras valores y restricciones siguen cubiertos por tests de configuración— y registrar ADRs cuando aparezcan decisiones arquitectónicas nuevas.

## Secuencia recomendada de PRs

1. **Documentación y baseline:** traslado, estado de docs, medición reproducible y freeze de `live`.
2. **Contrato de fills:** parser estricto y matriz de estados.
3. **Outbox durable:** state machine, idempotencia, crash/restart y reconciliación.
4. **Transporte, secretos y autorización:** TLS/WSS, allowlists, firma, permisos y redacción.
5. **Calendario y reloj:** sesiones BYMA, cache con procedencia y zona IANA.
6. **VIX y WebSocket:** frescura completa, flag de desactivación y límites de recursos.
7. **Evidencia:** registry de datasets, roles, holdout sellado y controles de leakage.
8. **Riesgo y metadata:** poder de compra, moneda, multiplicador e integridad de cadena.
9. **Validación cuantitativa:** oracle, propiedades, costos y simulador de ejecución realista.
10. **Consolidación operativa:** documentos, runbook, CI, observabilidad y ejercicios de recuperación.

Cada PR debe incluir tests primero o junto al cambio, actualizar su contrato documental y conservar compatibilidad/migración explícita. No conviene agrupar estas etapas en un refactor único porque dificultaría demostrar qué riesgo cerró cada modificación.

## Decisiones que no se recomiendan

- No agregar fallback a market order.
- No retransmitir una orden ambigua “por si acaso”.
- No asumir que un cierre de proceso liquidó una posición.
- No usar un VIX demorado como si fuera vigente.
- No permitir que un replay arbitrario habilite dinero real.
- No interpretar WebSocket conectado como mercado abierto o capacidad de operar.
- No promover TWAP, límite dinámico o vertical a real sin contrato del broker, profundidad y recuperación demostrados.
- No corregir documentación para que coincida con una conducta insegura; en ese caso se corrige Rust y el documento conserva el invariante.

## Criterios de salida para considerar `live`

Todos deben cumplirse:

- cero hallazgos P0 y P1 de ejecución/persistencia abiertos;
- state machine de orden idempotente y durable validada mediante crash/restart;
- reconciliación completa por ID y procedimiento probado para `UnknownTerminal`;
- transporte, secretos, autorización y permisos revisados;
- calendario bursátil vigente, cacheado y fail-closed;
- procedencia inmutable, holdout sellado y ausencia de solapamiento demostrada;
- costos, liquidez, no-fill, parciales y estrés incluidos en evaluación;
- gates de cobertura/mutación alcanzados sin exclusiones artificiales;
- documentación y runbook comprobados desde un entorno limpio;
- período shadow exitoso, canary mínimo con límites firmados y aprobación humana;
- rollback ensayado preservando schema, journal, órdenes y posiciones.

## Fuentes externas de autoridad

- [BYMA — Calendario bursátil](https://www.byma.com.ar/mercado/calendario-bursatil): distingue tipos de jornada y eventos propios del mercado.
- [BYMA — Horarios](https://www.byma.com.ar/mercado/horarios): referencia para sesiones y segmentos; debe archivarse la versión usada por cada manifest.
- [ArgentinaDatos — Feriados 2026](https://api.argentinadatos.com/v1/feriados/2026): fuente de feriados civiles, útil como señal auxiliar pero no sustituto del calendario de rueda.
- [Cboe — VIX](https://www.cboe.com/tradable-products/vix): los valores públicos se publican con una demora de al menos 20 minutos.
- [Cboe — Global Indices Feed](https://www.cboe.com/data/global-indices-feed): referencia para acceso licenciado a índices en tiempo real.
- [TruffleHog OSS — acción oficial](https://github.com/trufflesecurity/trufflehog/blob/main/action.yml): contrato de inputs y ejecución del scanner usado por CI.
- [CycloneDX Rust Cargo](https://github.com/CycloneDX/cyclonedx-rust-cargo): generador oficial del SBOM de dependencias Cargo.
