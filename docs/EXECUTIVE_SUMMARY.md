# Resumen ejecutivo

## Estado

El proyecto implementa observación/replay, paper trading, una TUI y una ruta real fuertemente gated. La estrategia acompaña tendencias: CALL ante suba confirmada y PUT ante baja confirmada. Todas las órdenes son limitadas.

La revisión de agosto de 2026 corrigió fallas materiales de fills, reenvío ambiguo, durabilidad, transporte, calendario, WebSocket, VIX, secretos, autorización y archivos. Las pruebas automatizadas y `clippy -D warnings` deben permanecer verdes; la cantidad/cobertura vigente se publica como evidencia de CI, no como promesa fija en este documento.

## Controles relevantes

- Estado de orden validado por matriz y seguimiento REST hasta terminal.
- Intención durable previa al efecto externo; no hay re-POST automático ambiguo.
- Calendario BYMA explícito y fail-closed para dinero real.
- WebSocket opcional, acotado y no autoritativo.
- Contraseña v2 con clave aleatoria externa; grant efímero firmado y anti-replay.
- Locks del kernel, permisos privados, escrituras atómicas y journal encadenado.
- Replay no sellado excluido del gate real.
- Estado de cuenta operable y fondos inmediatos verificados antes de cada posible entrada real.

## Riesgos residuales

`live` no debe considerarse listo sólo porque compile o conecte. Siguen requiriendo evidencia contractual/operativa, según `PLAN.md`, manifiestos bursátiles y de datos sellados, pruebas de proceso/crash más amplias, cobertura alta por riesgo y un canary supervisado. El endpoint oficial de alta, el estado de cuenta y la metadata contractual por instrumento ya fallan cerrado, pero eso no demuestra rentabilidad ni suficiencia operativa.

No hay rentabilidad demostrada, SLA, RTO/RPO medido ni garantía de ejecución. Readonly es el modo por defecto y el único recomendado hasta cerrar los criterios de salida pendientes.
