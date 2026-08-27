# Contratos técnicos implementados

`ALGORITMO.md` define la conducta completa. Este documento resume invariantes comprobables del código.

## Señal y selección

- `Direction::Up` selecciona `OptionKind::Call`.
- `Direction::Down` selecciona `OptionKind::Put`.
- `Neutral` no abre posición.
- Apertura, mediodía, fin de semana, vencimiento, frescura, spread, volumen y riesgo pueden bloquear una entrada; no invierten su dirección.
- Toda orden es limitada. No existe fallback a market.

## Ejecución

La matriz estricta exige:

- `Pending`: cero fills.
- `PartiallyExecuted`: `0 < filled < requested`, precio e ID del broker.
- `Executed`: cantidad exacta, precio positivo finito e ID.
- `Rejected`: sin fills.
- `Cancelled`: puede preservar un parcial, pero nunca equivaler a fill completo.

Una transición no puede cambiar identidad, perder fills ni regresar desde un estado terminal. La intención se sincroniza antes del `POST`. Un `401` refresca credenciales pero no repite ese `POST`; el resultado queda ambiguo para conciliación. Aceptada la orden, REST consulta por `broker_order_id`; WS sólo puede adelantar el poll. Timeout causa cancelación controlada y nueva espera terminal.

El journal comprueba una sola identidad por evento: el `operation_id` exterior debe coincidir con la solicitud, ejecución, fill parcial o posición contenida. Esta validación ocurre antes del hash/escritura y nuevamente al leer, incluso para schemas legados sin HMAC; un terminal no puede resolver otra intención por una contradicción interna auténticamente serializada.

Al iniciar, el replay reconstruye cada orden desde una intención durable conocida. Vuelve a aplicar la matriz de ejecución a la solicitud original y comprueba continuidad de `broker_order_id`, fills no regresivos y transiciones de estado. También exige que un evento `partial_fill_exposure` coincida exactamente con el último estado y con `requested - filled = remaining`. Los terminales duplicados idénticos son idempotentes porque la ruta real puede persistir una aceptación ya terminal y su confirmación; cualquier terminal contradictorio falla cerrado. Sólo ejecución completa, rechazo o cancelación sin fills eliminan la orden de pendientes.

## Persistencia

- Snapshot v4; lectura compatible v1–v3.
- Journal v6 autenticado en `live`; lectura compatible v1–v5 y cadena SHA-256 v5 para readonly.
- Secuencias deben comenzar en 1 y ser contiguas.
- Los eventos v5 incluyen hash anterior y SHA-256 propio. Esto detecta corrupción/modificación, pero no autentica frente a un atacante capaz de recalcular toda la cadena.
- Journal: máximo 128 MiB y 1.000.000 de eventos.
- Replay: máximo 1 GiB y 1.000.000 de frames.
- Captura diaria: 256 MiB por archivo; analytics/telemetría e historial IV: 128 MiB por archivo.
- Los archivos sensibles se crean `0600`, directorios propios `0700`, no siguen symlinks al abrir y usan temporales únicos para reemplazo atómico.

## Seguridad

`OPTIONS_MASTER_KEY_PATH` apunta a una clave aleatoria Base64 de 32 bytes. `--init-master-key` la crea; `--encrypt-password` usa prompt sin eco, rechaza la entrada vacía y produce `IOL_PASSWORD=v3:...` con una subclave HKDF-SHA256 separada por contexto. `v2:` y el cifrado basado en machine-id se conservan sólo como lectura de migración readonly; `live` los rechaza.

La promoción a Canary requiere `LIVE_READINESS_PATH`: manifest v2 firmado con subclave separada, métricas mínimas cerradas, hashes de reportes normalizados de cobertura/mutación, archivo de corpus fuzz y build integral. La v2 incluye explícitamente identidad de build, verificador de readiness, filesystem seguro y referencia horaria dentro de la frontera crítica. La autorización v3 incorpora el hash del readiness firmado, además de cuenta, epoch, fingerprint, build, reporte y límites. Canary/Live revalidan ese mismo hash en cada ciclo y cada compra lo verifica otra vez antes de crear el intent durable y del `POST`; una pérdida de validez degrada a Learning al quedar plano sin bloquear una venta reductora de riesgo.

Los grants v3 incorporan nonce, cuenta, epoch, fingerprint, build, hash del readiness firmado, hash del reporte, límites y expiración. Una firma HMAC cubre el payload. El consumo reclama atómicamente un nombre derivado del nonce; copiar el mismo grant no permite reutilizarlo.

## Datos externos

- IOL REST y calendario exigen HTTPS; WS habilitado exige WSS.
- Redirects HTTP están deshabilitados.
- JSON IOL: 8 MiB; calendario: 512 KiB; VIX: 64 KiB; WS: mensaje 64 KiB/frame 16 KiB.
- El JSON IOL acepta exactamente `application/json` o subtipos `+json`; 8 MiB es una frontera inclusiva verificada antes y durante la descarga. OAuth requiere token no vacío, tipo Bearer y expiración positiva, con renovación preventiva a 30 s. Las lecturas de mercado sólo reintentan fallos HTTP/de transporte y el tercer fallo consecutivo abre un circuito de 300 s.
- VIX actual y cierre previo tienen timestamps y vigencias independientes. Sin dato actual vigente, el filtro no se muestra ni se aplica.
- `live` requiere manifiesto BYMA versionado y con cobertura inclusiva de la fecha. Una sesión BYMA explícita es autoritativa incluso en fin de semana y no depende del feed civil auxiliar. El calendario civil sólo complementa fechas sin excepción bursátil; sus fechas deben ser reales, únicas, pertenecer al año pedido y tener descripción. Timestamps no representables fallan cerrado y un fallo al persistir una respuesta civil válida queda registrado y obliga a revalidarla en el siguiente proceso.
- Cartera y pendientes IOL se decodifican de forma total para reconciliación: cualquier fila sin símbolo/cantidad válida, o pendiente sin ID/lado, invalida el snapshot; no hay descarte silencioso ni redondeo.
- La recuperación compara símbolo, cantidad y CALL/PUT. Al reconstruir una exposición, congela metadata íntegra y fresca del catálogo IOL; si falta, mantiene la posición localmente visible con freno operativo y nunca degrada una salida real a paper.
- El replay exige correspondencia exacta entre la única posición del motor y la del portfolio. Aperturas idénticas son idempotentes; exposiciones adicionales, contenido divergente y cierres de otro `operation_id` fallan cerrado. Un evento que libera el `KillSwitch` tampoco puede borrar un freno operativo pendiente.
- `live` verifica periódicamente el reloj local contra `TIME_REFERENCE_URL`, de origen HTTPS distinto de IOL; TTL y skew tienen fronteras exactas (`edad < TTL`, `skew <= máximo`). Una observación inválida no se cachea y el siguiente ciclo puede reintentar. Una falla bloquea entradas nuevas pero conserva la ruta de salida.

## Evidencia

Shadow prospectivo puede alimentar Learning. Canary y Live se evalúan separadamente. Replay sin procedencia/split sellados es `ResearchReplay`: conserva diagnósticos pero `LearningState::record` no lo acepta para habilitar dinero real.
