# Runbook de despliegue y operación

## Preparación

1. Instalar el toolchain Rust compatible con `Cargo.lock`.
2. Copiar `.env.example` a `.env` y mantener `MODE=readonly`.
3. Crear una clave fuera del repositorio y de `DATA_DIR`:

```bash
cargo run --locked -- --init-master-key /ruta/privada/options-master.key
export OPTIONS_MASTER_KEY_PATH=/ruta/privada/options-master.key
cargo run --locked -- --encrypt-password
```

4. Pegar el valor `v3:` en `IOL_PASSWORD`; completar usuario. Un valor `v2:` sólo sirve para migración readonly y debe volver a cifrarse antes de `live`.
5. Para una futura operación real, instalar un manifiesto de sesiones BYMA versionado, verificable y con cobertura vigente. No inventar fechas para satisfacer el gate.

## Verificación previa

```bash
cargo test --all-targets --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
```

Ejecutar primero readonly:

```bash
MODE=readonly cargo run --locked
```

Confirmar en TUI por separado `IOL`, `WS`, mercado y capacidad operativa. `IOL ONLINE` no significa que el mercado esté abierto ni que `live` esté autorizado.

## Habilitación real

No habilitar mientras `docs/PLAN.md` tenga criterios P0 abiertos aplicables al entorno. Además del gate estadístico se requieren `MODE=live`, confirmación exacta, `IOL_ORDER_PATH`, `LIVE_READINESS_PATH`, `LIVE_AUTHORIZATION_PATH`, `MARKET_SESSIONS_PATH`, `TIME_REFERENCE_URL` de origen HTTPS independiente de IOL, clave maestra y credenciales v2.

El readiness no se completa a mano con números estimados. Después de campañas reales, obtener el hash exacto con `cargo run --locked -- --print-build-hash`, preparar el manifest v2 con las métricas y hashes observados, y firmarlo contra los mismos archivos:

```bash
cargo run --locked -- --sign-release-readiness \
  readiness-input.json readiness-coverage.json readiness-mutation.json fuzz-evidence.tar.zst \
  data/live/release-readiness.json
```

La utilidad compara las métricas del manifest con los reportes normalizados y liga también el archivo del corpus. El runtime vuelve a verificar firma, antigüedad, build, scopes y umbrales en cada ciclo de Canary/Live. Cambiar código o readiness invalida la autorización y provoca retorno conservador a Learning al quedar plano.

Cuando se genere `data/live/live-authorization-request.json`, revisarlo y emitir un grant en otra invocación:

```bash
cargo run --locked -- --authorize-live \
  data/live/live-authorization-request.json \
  data/live/live-authorization.json
```

El grant vence en 15 minutos, está firmado y se consume una vez.
La utilidad rechaza la solicitud antes de pedir confirmación si sus hashes, límites, cuenta o epoch son inválidos, o si pertenece a otro build. La terminal muestra la cuenta enmascarada; la revisión exacta se hace sobre el request privado.

## Detención

`q`, `Esc` o `Ctrl+C` solicitan shutdown y persistencia. El proceso no vende automáticamente por salir. Si existe una posición, una orden ambigua o una conciliación bloqueada, verificar IOL manualmente antes de reiniciar o declarar el cierre limpio.

## Recuperación

1. Mantener el motor detenido y revisar cartera/órdenes en IOL.
2. Conservar journal, snapshot y archivos consumidos; no editarlos para forzar el arranque.
3. En una migración desde una versión anterior, verificar `0700` en directorios de estado y `0600` en snapshot, journal, historiales, claves y autorizaciones. El runtime falla cerrado ante permisos privados más amplios; corregirlos sólo después de comprobar propietario y contenido.
4. Corregir la causa externa o configuración.
5. Reiniciar con `RECOVER_STATE=true`; una secuencia/hash inválidos falla cerrado.
6. Si persiste ambigüedad, continuar readonly y conciliar manualmente. No repetir un POST dudoso.

No existen migraciones de base de datos, health endpoint ni cierre automático de posiciones en el runtime actual.
