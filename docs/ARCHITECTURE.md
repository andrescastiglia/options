# Arquitectura implementada

## Alcance

El binario es un motor monolítico Rust/Tokio con TUI opcional. Opera en `readonly` o `live`; replay es una fuente de mercado dentro de readonly y nunca habilita órdenes reales. No hay base de datos, servicio HTTP propio, hot reload ni health endpoint.

## Componentes

- `main`: carga `.env`, procesa utilidades seguras, ejecuta preflight y administra TUI/headless y shutdown.
- `app`: orquesta calendario, señal, selección, riesgo, órdenes, conciliación, evidencia y persistencia.
- `iol_client`: OAuth, REST IOL, parsing acotado, seguimiento de orden y WebSocket opcional de movimientos.
- `market` y `market_calendar`: contratos de cotización/replay, frescura y sesión argentina/BYMA.
- `pattern`, `trading`, `risk` y `portfolio`: tendencia, estado de posición, límites y P&L.
- `learning` y `learning_model`: evidencia prospectiva, gate y meta-filtros experimentales.
- `persistence` y `secure_fs`: journal, snapshot, locks y archivos privados.
- `tui`: vista operativa; nunca es autoridad para confirmar una ejecución.

## Flujo de datos

```text
IOL REST o replay
      |
      v
MarketFrame validado --> calendario/frescura --> detector de tendencia
                                              |
                                Up -> CALL; Down -> PUT
                                              |
                         selección + riesgo + gate
                                              |
                       paper broker o REST IOL limitado
                                              |
                     journal -> portfolio -> snapshot/TUI
```

La dirección acompaña al subyacente: nunca se invierte una señal alcista para comprar PUT ni una bajista para comprar CALL.

## Fronteras de confianza

- REST es la autoridad de mercado, cuenta y estado de orden. El WebSocket sólo puede adelantar una consulta REST por `broker_order_id` verificable.
- `live` exige HTTPS, WSS si se habilita WS, calendario de sesiones versionado, referencia horaria de origen independiente, ruta de orden explícita, clave maestra, contraseña `v3:` derivada con HKDF-SHA256 y autorización firmada.
- Una respuesta ambigua no causa reenvío automático del `POST`.
- Replay ordinario es investigación no sellada y no alimenta el gate real.

## Persistencia y concurrencia

Cada modo mantiene un lock exclusivo del kernel. El journal append-only usa secuencia continua; v5 agrega cadena SHA-256 y v6 autentica con HMAC obligatorio en `live`. La intención de una orden real se sincroniza antes del efecto externo y el ID del broker se persiste al aparecer. Snapshots y JSON atómicos usan temporales únicos, `rename`, sync de directorio y permisos privados.

El proceso usa un solo orquestador y tareas Tokio para I/O. Las solicitudes HTTP se limitan por semáforo; las colas WS son acotadas.

## Shutdown

Salir detiene entradas, cierra la tarea WS, sincroniza journal y guarda snapshot. No liquida automáticamente una posición. Un shutdown sólo queda marcado limpio si el estado local es conciliable y no existe una posición que requiera supervisión.
