# Seguridad y modelo de amenazas

## Alcance

El sistema procesa credenciales, datos privados de cuenta y órdenes con efecto económico. Se asume que respuestas de red, archivos de datos, variables de entorno y mensajes WebSocket pueden ser malformados o quedar desactualizados. Un usuario con control completo del proceso o de la clave maestra queda fuera del perímetro; la separación de permisos del sistema operativo sigue siendo obligatoria.

## Controles vigentes

- IOL exige HTTPS; el WebSocket, si se habilita, exige WSS. No se siguen redirects y los bodies/frames tienen límites.
- Los valores opcionales vacíos se tratan como ausentes: un `REPLAY_PATH` blanco no omite credenciales, usuario o contraseña blancos no autentican y la confirmación de dinero real exige coincidencia byte a byte. Esto impide que espacios aparenten satisfacer un gate.
- La contraseña vigente `v3:` usa una clave maestra aleatoria externa indicada por `OPTIONS_MASTER_KEY_PATH` y una subclave HKDF-SHA256 separada por contexto. Rechaza secretos vacíos. La opción CLI `-e contraseña` fue retirada; `--encrypt-password` solicita el secreto sin eco. `v2:` y el formato ligado a machine-id sólo se descifran para migración readonly y nunca habilitan `live`.
- La autorización real es efímera, ligada a cuenta, epoch, fingerprint y límites, firmada con HMAC y consumida una sola vez mediante nonce. La utilidad de emisión rechaza campos ambiguos, hashes no canónicos, límites incoherentes y requests de otro build antes de solicitar confirmación; la cuenta sólo aparece enmascarada en terminal.
- Canary exige además readiness HMAC v2 ligado al hash de todo `src/`, manifest/lockfile/toolchain y a los bytes exactos de los reportes normalizados de cobertura/mutación y del archivo de corpus fuzz. La autorización v3 liga ese readiness; una modificación de código o evidencia exige repetir la aprobación. Cada compra lo vuelve a verificar justo antes del intent/POST para cerrar la ventana entre ciclos; una salida reductora de riesgo no depende de renovar ese permiso.
- Los manifiestos de datasets usan una subclave HMAC separada por contexto, archivo raw verificado por SHA-256, registro privado inmutable y marca de consumo único del holdout. Un manifiesto firmado prueba control de la clave e identidad de bytes; no prueba por sí solo calidad, licencia o independencia metodológica, que requieren revisión humana.
- `DATA_DIR` y archivos sensibles fuerzan permisos privados, rechazan symlinks y usan locks de kernel por raíz/modo y escrituras atómicas con temporales únicos. La raíz tiene cuota agregada, reserva de espacio y una única instancia. La telemetría prescindible se segmenta por día; el TUI y `telemetry/storage-pressure.json` exponen uso, límite y espacio libre.
- El cuerpo HTTP exacto de cada catálogo IOL usado como metadata contractual se archiva por SHA-256 antes de normalizarlo. Una entrada real exige schema conocido, digest no nulo y archivo verificado; reutilizar un nombre cuyo contenido fue alterado falla cerrado. Estos archivos participan de la cuota agregada y no se eliminan automáticamente.
- `live` exige una referencia horaria HTTPS de origen distinto de IOL. Un fallo, encabezado `Date` inválido o desvío fuera del límite bloquea entradas nuevas sin impedir salidas de riesgo.
- La intención de orden se sincroniza antes del `POST`; una respuesta ambigua nunca provoca un reenvío automático. En `live`, cada evento del journal v6 lleva HMAC con una subclave separada y la lectura rechaza clave incorrecta, alteración o cadena recomputada por un actor sin clave. La identidad exterior de cada evento también debe coincidir con su payload tipado. Al reiniciar, un terminal sólo resuelve una intención previa si conserva ID broker, cantidad y transición; estados huérfanos, regresiones y parciales incoherentes fallan cerrado.
- Logs, TUI y errores terminales pasan por un saneador central acotado que elimina marcadores de credenciales, controles, cuentas de seis o más dígitos, correos e identificadores externos. Su escaneo consume siempre al menos un carácter por iteración, incluso ante entradas adversariales. Una prueba de contrato audita que los sinks de texto libre sigan aplicándolo. El detalle técnico no sensible vive en el historial operativo, no en el encabezado.
- El toolchain y lockfile están fijados; CI aplica RustSec, dependency review, Dependabot, `cargo-deny`, TruffleHog fijado por commit/imagen y un SBOM CycloneDX 1.5 archivado. La policy rechaza dependencias wildcard, fuentes desconocidas, crates retirados y licencias fuera de la allowlist.

## Fallo seguro

Se bloquean nuevas entradas ante calendario bursátil desconocido, cotización vencida, fondos no verificables, metadata contractual ausente, estado de orden contradictorio, autorización inválida, divergencia entre cuenta y estado local o falta de durabilidad. El replay rechaza más de una exposición o cualquier diferencia entre motor y portfolio, y no libera un freno operativo mediante un evento histórico de `KillSwitch`. Las salidas de riesgo siguen intentándose cuando existe un bid ejecutable; nunca se inventa una ejecución.

## Riesgos residuales

Las fronteras de escritura atómica tienen fault injection, la presión de cuota se prueba con archivos reales y un harness termina procesos en cada frontera durable del protocolo de orden, incluida cancelación. Faltan campañas prolongadas de fuzzing/mutación y una certificación contra sandbox oficial del broker. La autenticación HMAC protege el journal mientras la clave maestra permanezca fuera del alcance del atacante, pero no protege ante compromiso total de proceso y clave. Los captures se podan por retención, pero no deben conservarse datos reales sin revisar minimización, licencia y acceso.

## Respuesta operativa

Ante una orden desconocida, inconsistencia, filtración o corrupción: impedir nuevas entradas, conservar artefactos privados sin editarlos, verificar manualmente la cuenta en IOL, rotar credenciales/clave si corresponde y reconciliar por `broker_order_id`. No borrar ni “corregir” el journal antes de preservar evidencia.
