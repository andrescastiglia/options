# Documentación

Estado verificado el 25 de agosto de 2026.

## Fuentes canónicas

- [`ALGORITMO.md`](ALGORITMO.md): contrato funcional, precedencias, estados y defaults.
- [`PLAN.md`](PLAN.md): auditoría, riesgos, trabajo realizado y criterios pendientes.
- [`../README.md`](../README.md): instalación y operación básica.

## Documentos técnicos

- [`ARCHITECTURE.md`](ARCHITECTURE.md): componentes y fronteras efectivamente implementados.
- [`IMPLEMENTATION_DETAILS.md`](IMPLEMENTATION_DETAILS.md): contratos técnicos de órdenes, persistencia y datos.
- [`DATA_CONTRACTS.md`](DATA_CONTRACTS.md): schemas y procedencia exigida a calendarios, VIX y replay.
- [`SECURITY.md`](SECURITY.md): modelo de amenazas, controles, fallo seguro y respuesta operativa.
- [`TESTING.md`](TESTING.md): política sin trampas, suites, comandos y gates pendientes.
- [`DEPLOYMENT.md`](DEPLOYMENT.md): runbook de despliegue, arranque, detención y recuperación.
- [`EXECUTIVE_SUMMARY.md`](EXECUTIVE_SUMMARY.md): estado ejecutivo y riesgos residuales sin promesas no medidas.

## Informes de entrada

- `ANALISIS.md` y `ANALISIS2.md` fueron reportes externos usados como insumo y ya no están versionados. No son contrato del runtime.

Ante una divergencia, prevalece el código probado. La corrección debe actualizar en el mismo cambio `ALGORITMO.md`, tests y el documento técnico afectado.
