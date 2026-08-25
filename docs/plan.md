# Readonly y live con ciclo automático

## Objetivo

Sólo existen dos modos públicos y ambos consumen mercado real de IOL:

- `readonly`: jamás envía órdenes. Simula fills conservadores para aprender, muestra
  cuándo debería comprar o vender y mide el resultado virtual.
- `live`: durante Learning también simula. Sólo Canary/Live envían órdenes reales,
  con autorización efímera consumible y contrato HTTP de IOL configurado.

El ciclo persistente es `Learning → Eligible → Armed → Canary → Live`, con retorno
fail-closed a Learning. Readonly se detiene en Eligible y nunca puede mover dinero.

## Compra y venta CALL/PUT

Una tendencia alcista robusta compra virtual o realmente una CALL; una tendencia
bajista hace lo mismo con una PUT. Sólo se compran opciones, nunca se lanzan. Cada
posición se vende por objetivo neto, stop, reversión confirmada, timeout o cierre
manual. Una reversión impone cinco minutos de cooldown y exige una señal nueva.

## Objetivo económico y pérdida asumida

- La compra paga su costo operativo sobre la prima de entrada.
- La venta paga nuevamente su costo operativo sobre la prima de salida.
- El costo operativo efectivo incluye comisión, otros cargos e IVA:
  `(comisión + otros cargos) × (1 + IVA / 100)`.
- El impuesto estimado a la ganancia se aplica sólo si el resultado bruto es
  positivo; no se inventa un impuesto separado sobre una pérdida.
- El P&L neto es `bruto - costo_compra - costo_venta - impuesto_ganancia`.
- El stop usa el primero entre la caída porcentual y `MAX_LOSS_PER_TRADE` neto.
- El objetivo neto cubre como mínimo el mayor entre costos de ida y vuelta por
  `MIN_PROFIT_MULTIPLIER` y riesgo neto por `MIN_REWARD_RISK_RATIO`.
- La cantidad es el menor límite entre efectivo, contratos máximos y riesgo.

## Filtros, calibración y calentamiento

Antes de una señal operable se exigen bid/ask válidos, datos frescos, spread ≤ 3%,
volumen ≥ 10, vencimiento entre 7 y 45 días y distancia al dinero ≤ 10%. La selección
prioriza menor fricción, mayor volumen, cercanía al dinero y a 21 días.

Los costos se calibran sólo con operaciones terminadas identificadas como opciones.
La calibración se persiste, vence a los 30 días y cualquier cambio inicia un nuevo
epoch. La señal necesita la ventana histórica completa, 30 confirmaciones,
separación SMA ≥ 0,10%, pendiente normalizada ≥ 0,02% por minuto, `R² ≥ 0,60` y
movimiento de al menos una desviación estándar.

## Learning → Eligible → Armed → Canary → Live

La evidencia compatible se comparte por fingerprint de estrategia/build/gate. Un epoch necesita como mínimo 200 cierres, 75
CALL, 75 PUT y 20 ruedas; resultado y expectativa positivos; profit factor ≥ 1,25
total y por lado; bootstrap 95% positivo; drawdowns dentro de los límites y resultado
positivo duplicando costos/slippage.

La promoción a Eligible ocurre estando plano, con datos y calibración vigentes,
calentamiento completo y kill switch inactivo. `live` exige además cuenta IOL
reconciliada, `LIVE_TRADING_CONFIRMATION`, `IOL_ORDER_PATH` y un grant de 15 minutos
ligado a cuenta/epoch/build/reporte. El grant se consume al pasar a Armed; Canary usa
límites reducidos y debe aprobar su propio gate antes de Live.

## Degradación → Learning

Ambos modos vuelven automáticamente a Learning al quedar planos ante tres pérdidas
consecutivas, expectativa no positiva, profit factor reciente menor que uno,
drawdown excesivo o calibración cambiada/vencida. El nuevo epoch comienza en cero.
Una inconsistencia o una orden real parcial activa el bloqueo operativo y requiere
intervención.

## Validación

Las pruebas deben demostrar aislamiento de readonly, simetría CALL/PUT,
calentamiento, filtros, costos independientes de compra y venta, IVA en ambos,
impuesto sólo sobre ganancias, objetivo posterior a costos/slippage, persistencia y
transiciones automáticas. La aceptación requiere `cargo test --all-targets --locked`,
`cargo fmt --check` y `cargo clippy --all-targets --locked -- -D warnings`.
