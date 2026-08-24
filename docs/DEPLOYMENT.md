# Guía de Deployment y Operación

---

## 1. Preparación del Ambiente

### 1.1 Requisitos Previos

```plaintext
✓ Rust 1.70+ (rustup)
✓ Cargo (incluido en Rust)
✓ Git
✓ (Opcional) SQLite3 (solo para testing local si se desea)
✓ Acceso a Invertir Online (usuario y contraseña)
✓ Token de acceso y refresh de IOL (obtener vía OAuth)
```

### 1.2 Instalación Inicial

```bash
# Clonar repositorio
git clone <repo-url>
cd proyecto-options-trading

# Crear archivo .env (basado en .env.example)
cp .env.example .env

# Editar .env con credenciales reales
nano .env
# O tu editor preferido: vim, code, etc.

# Verificar Rust
rustc --version
cargo --version

# Compilar en modo debug (para desarrollo)
cargo build

# Compilar en modo release (para producción)
cargo build --release
```

### 1.3 Estructura de .env

```plaintext
# === CREDENCIALES IOL ===
IOL_USERNAME=tu_usuario_iol
IOL_PASSWORD=base64_generado_con_-e
IOL_REFRESH_TOKEN=token_refresh_inicial

# === PARÁMETROS DE OPERACIÓN ===
TICKER=GGAL
CHECK_INTERVAL_SECS=5
PRICE_HISTORY_MINUTES=30
MIN_SAMPLES_FOR_TREND=5
TREND_CHANGE_SAMPLES=3

# === PARÁMETROS ECONÓMICOS ===
COMMISSION_PERCENTAGE=0.19
TAX_PERCENTAGE=35
MIN_PROFIT_MULTIPLIER=2.0
OPTION_EXPIRY_DAYS=1

# === PARÁMETROS TÉCNICOS ===
LOG_LEVEL=info
MAX_CONCURRENT_REQUESTS=10
CACHE_TTL_SECS=60
MAX_POSITION_SIZE=5
POSITION_TIMEOUT_MINS=60
MAX_MARKET_DATA_AGE_SECS=15
MAX_OPTION_SPREAD_PERCENTAGE=20

# === PERSISTENCIA ===
# Por defecto se usa in-memory + journal/snapshots. Si se desea un backend externo, habilitar DATABASE_URL.
# Ejemplo (opcional): DATABASE_URL=sqlite:///home/user/data/trading.db  # Solo si se usa un backend externo (no requerido por defecto)
```

---

## 2. Persistencia en memoria y snapshots (no DB por defecto)

No es necesario inicializar una base de datos. Por defecto el bot mantiene el estado en memoria y ofrece dos mecanismos opcionales para durabilidad y auditoría:

1) Journal append-only (`DATA_DIR/<modo>/journal.jsonl`): cada evento operativo se escribe tipado y secuenciado.
2) Snapshot (`DATA_DIR/<modo>/state.json`): estado completo escrito mediante archivo temporal y rename atómico.

### 2.1 Preparar directorio de snapshots/journal

```bash
# Crear directorios
mkdir -p ./data/readonly ./data/live
chmod 700 ./data/readonly ./data/live
```

### 2.2 Verificación rápida

```bash
# Verificar que la app puede escribir snapshots (simulación)
TUI_ENABLED=false DATA_DIR=./data cargo run
test -s ./data/readonly/journal.jsonl
test -s ./data/readonly/state.json
```

# Opcional: Si se desea usar SQLite/Postgres/otro backend, habilitar con la variable DATABASE_URL y proveer el script SQL correspondiente (no usado por defecto).


---

## 3. Validación Pre-Lanzamiento

### 3.1 Checklist de Configuración

```plaintext
□ .env existe y tiene permisos 600 (no world-readable)
□ IOL_USERNAME es correcto e IOL_PASSWORD contiene el Base64 generado con `-e` en esta máquina
□ IOL_REFRESH_TOKEN está presente
□ TICKER es válido en IOL (GAL, GGAL, MERV, etc.)
□ CHECK_INTERVAL_SECS >= 1 y <= 60
□ MIN_SAMPLES_FOR_TREND >= 2
□ MIN_PROFIT_MULTIPLIER >= 1.0
□ (Opcional) DATABASE_URL es accesible si usa un backend externo
□ LOG_LEVEL es válido (debug|info|warn|error)
□ Log directory tiene permisos de escritura
```

### 3.2 Test de Conexión a IOL

```bash
# En código Rust (test_iol_connection)
cargo test test_iol_connection -- --nocapture

# Deberías ver:
# [INFO] Conectando a IOL...
# [DEBUG] Autenticación OK, token válido por XXX minutos
# [INFO] Obtuviendo opciones de GAL...
# [DEBUG] Encontradas 15 opciones disponibles
```

### 3.3 Test de Persistencia (Opcional)

Si usas el modo por defecto (in-memory + snapshots/journal), validar que la aplicación puede escribir en ./data/journal y ./data/snapshots y que los permisos son correctos. Si usas un backend SQL, ejecutar los tests de DB apropiados.

```bash
# Test simple de escritura de journal (simulación)
cargo test test_journal_write -- --nocapture || true

# Si usas un DB backend, ejecutar:
cargo test test_database_write -- --nocapture
```
---

## 4. Ejecución Local

### 4.1 Modo Debug (Desarrollo)

```bash
# Compilar y ejecutar con logs detallados
RUST_LOG=debug cargo run

# Espera ver:
# [2026-08-19T18:15:30] INFO  Iniciando Trading Bot v1.0
# [2026-08-19T18:15:31] INFO  Configuración cargada (TICKER=GAL)
# [2026-08-19T18:15:32] DEBUG Conectando a IOL...
# [2026-08-19T18:15:33] INFO  Autenticación exitosa
# [2026-08-19T18:15:34] DEBUG Iniciando monitoreo de precios...
# [2026-08-19T18:15:39] DEBUG Precio: $100.50, SMA: $100.35
# ...
```

### 4.2 Modo Release (Performance)

```bash
# Compilar optimizado
cargo build --release

# Ejecutar versión optimizada
./target/release/options-trading

# Más rápido y bajo consumo de memoria
```

### 4.3 Parar el Bot

```bash
# Presionar Ctrl+C (SIGINT)
# El bot debería:
# 1. Cerrar posiciones abiertas (si las hay) o marcar en journal
# 2. Guardar snapshot de estado
# 3. Flush journal a disco
# 4. Loguear: "Bot detenido correctamente"

^C
# [2026-08-19T18:30:15] INFO  Señal de terminación recibida
# [2026-08-19T18:30:16] INFO  Cerrando posición activa CALL GAL 105...
# [2026-08-19T18:30:18] INFO  Guardando snapshot de estado
# [2026-08-19T18:30:19] INFO  Bot detenido correctamente
```

---

## 5. Deployment en Producción

### 5.1 Opción A: Servidor Linux (Recomendado)

#### Paso 1: Preparar servidor

```bash
# Conectar al servidor
ssh usuario@servidor.com

# Actualizar paquetes
sudo apt update && sudo apt upgrade -y

# Instalar dependencias
sudo apt install -y rustc cargo tmux git

# Crear usuario específico para bot
sudo useradd -m -s /bin/bash trading-bot

# Cambiar a usuario
sudo su - trading-bot
```

#### Paso 2: Deploy código

```bash
# En home de trading-bot
cd ~
git clone <repo-url> trading-app
cd trading-app

# Compilar para producción
cargo build --release

# Crear directorio de datos
mkdir -p ~/data
chmod 700 ~/data

# Configurar .env
cp .env.example .env
nano .env  # Editar con credenciales reales
chmod 600 .env
```

#### Paso 3: Ejecutar como servicio

```bash
# Opción A: systemd (recomendado)
sudo nano /etc/systemd/system/trading-bot.service
```

Contenido:
```plaintext
[Unit]
Description=Crypto Trading Bot
After=network.target

[Service]
Type=simple
User=trading-bot
WorkingDirectory=/home/trading-bot/trading-app
ExecStart=/home/trading-bot/trading-app/target/release/options-trading
Restart=always
RestartSec=10
StandardOutput=journal
StandardError=journal
Environment="RUST_LOG=info"

[Install]
WantedBy=multi-user.target
```

```bash
# Habilitar y arrancar servicio
sudo systemctl daemon-reload
sudo systemctl enable trading-bot
sudo systemctl start trading-bot

# Ver estado
sudo systemctl status trading-bot

# Ver logs en tiempo real
sudo journalctl -u trading-bot -f

# Detener servicio
sudo systemctl stop trading-bot
```

#### Opción B: tmux (desarrollo/testing)

```bash
# Crear sesión tmux
tmux new-session -d -s trading

# Ejecutar bot en sesión
tmux send-keys -t trading "cd ~/trading-app && ./target/release/options-trading" Enter

# Monitorear
tmux attach -t trading

# Detener (dentro de tmux): Ctrl+C
# Salir sin matar: Ctrl+B luego D
```

### 5.2 Opción B: Docker (Containerizado)

#### Paso 1: Crear Dockerfile

```dockerfile
FROM rust:1.70 as builder

WORKDIR /app
COPY . .

RUN cargo build --release

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y ca-certificates
RUN rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/options-trading /usr/local/bin/

WORKDIR /data
RUN chown 1000:1000 /data

USER 1000

CMD ["options-trading"]
```

#### Paso 2: Build y Run

```bash
# Compilar imagen
docker build -t trading-bot:latest .

# Ejecutar contenedor
docker run -d \
  --name trading-bot \
  --restart always \
  --env-file .env \
  -v trading-data:/data \
  trading-bot:latest

# Ver logs
docker logs -f trading-bot

# Detener
docker stop trading-bot
```

#### Paso 3: Docker Compose (Recomendado)

```yaml
# docker-compose.yml
version: '3.8'

services:
  trading-bot:
    build: .
    container_name: trading-bot
    restart: always
    env_file: .env
    volumes:
      - trading-data:/data
      - ./logs:/app/logs
    logging:
      driver: "json-file"
      options:
        max-size: "10m"
        max-file: "3"

volumes:
  trading-data:
```

```bash
# Ejecutar
docker-compose up -d

# Monitorear
docker-compose logs -f trading-bot

# Detener
docker-compose down
```

---

## 6. Monitoreo en Producción

### 6.1 Logs Importantes

```bash
# Último estado del bot
tail -f /var/log/trading-bot/app.log

# Buscar errores
grep ERROR /var/log/trading-bot/app.log | tail -20

# Hoy's operaciones
grep $(date +%Y-%m-%d) /var/log/trading-bot/app.log

# Búsqueda de eventos específicos
grep "COMPRA\|VENTA" /var/log/trading-bot/app.log
```

### 6.2 Métricas Diarias

```bash
# Ejemplo: métricas diarias usando journal (JSONL) con jq
# Contar operaciones hoy
TODAY=$(date +%Y-%m-%d)
jq -s --arg day "$TODAY" '[.[] | select(.created_at | startswith($day))] | length' ./data/journal/*.jsonl

# Sumar P&L neto hoy
jq -s --arg day "$TODAY" '[.[] | select(.created_at | startswith($day)) | .pnl_net] | add' ./data/journal/*.jsonl

# Listar últimas 5 operaciones
tail -n 5 ./data/journal/*.jsonl | jq -c .
```

### 6.3 Alertas Recomendadas

**Crear scripts de alerta:**

```bash
#!/bin/bash
# check-bot-health.sh

# Verificar que bot está corriendo
if ! pgrep -f "options-trading" > /dev/null; then
    echo "⚠️  ALERTA: Trading Bot NO está corriendo"
    # Intentar reiniciar
    systemctl start trading-bot
    # Notificar (email, Slack, etc)
fi

# Verificar conexión a IOL (último precio < 5 minutos)
# Verificar conexión a IOL (último snapshot o journal)
# Usar timestamp del último snapshot o la última entrada del journal
LAST_SNAPSHOT=$(ls -1t ./data/snapshots 2>/dev/null | head -1 || true)
if [ -n "$LAST_SNAPSHOT" ]; then
  LAST_PRICE_TIME=$(stat -c %Y ./data/snapshots/$LAST_SNAPSHOT)
else
  # leer timestamp de la última entrada del journal (si existe)
  LAST_PRICE_TIME=$(tail -n 1 ./data/journal/*.jsonl 2>/dev/null | jq -r 'select(.timestamp != null) | .timestamp' 2>/dev/null || true)
fi

# Si más de 5 minutos sin actualización
if [ -n "$LAST_PRICE_TIME" ]; then
  # convertir a epoch si es necesario (simplificado)
  echo "Última actualización: $LAST_PRICE_TIME"
else
  echo "⚠️  ALERTA: Sin datos de IOL en últimos 5 minutos"
fi

# Verificar posición abierta > TIMEOUT: inspeccionar journal para operaciones abiertas
ABANDONED=$(jq -s '[.[] | select(.state=="ACTIVE" and (.created_at < (now|tostring))) ] | length' ./data/journal/*.jsonl 2>/dev/null || echo 0)

if [ "$ABANDONED" -gt 0 ]; then
    echo "⚠️  ALERTA: Posición abierta hace más de 60 minutos"
fi
```

Ejecutar con cron:
```bash
# Cada 5 minutos
*/5 * * * * /home/trading-bot/check-bot-health.sh
```

---

## 7. Mantenimiento

### 7.1 Rotación de Logs

```bash
# Si no usas systemd journal, rotar logs manualmente
# Crear archivo de logrotate

sudo nano /etc/logrotate.d/trading-bot
```

Contenido:
```plaintext
/var/log/trading-bot/*.log {
    daily
    rotate 30
    compress
    delaycompress
    notifempty
    create 0644 trading-bot trading-bot
    postrotate
        systemctl reload trading-bot > /dev/null 2>&1 || true
    endscript
}
```

### 7.2 Backup de Datos

```bash
#!/bin/bash
# daily-backup.sh

DATE=$(date +%Y%m%d_%H%M%S)
BACKUP_DIR="/backups/trading-bot"

mkdir -p $BACKUP_DIR

# Backup de snapshots, journal y logs
cp -a ./data/snapshots $BACKUP_DIR/snapshots_$DATE
cp -a ./data/journal $BACKUP_DIR/journal_$DATE

# Backup de logs
tar -czf $BACKUP_DIR/logs_$DATE.tar.gz ./logs/

# Mantener últimas 30 copias (snapshots/journal)
find $BACKUP_DIR -name "snapshots_*" -mtime +30 -exec rm -rf {} +
find $BACKUP_DIR -name "journal_*" -mtime +30 -exec rm -rf {} +
find $BACKUP_DIR -name "logs_*.tar.gz" -mtime +30 -delete

echo "Backup completado: $BACKUP_DIR"
```

```bash
# Ejecutar diariamente a las 2 AM
0 2 * * * /home/trading-bot/daily-backup.sh
```

### 7.3 Actualización de Bot

```bash
# Parar bot
sudo systemctl stop trading-bot

# Actualizar código
cd ~/trading-app
git pull origin main

# Recompilar
cargo build --release

# Reiniciar
sudo systemctl start trading-bot

# Verificar
sudo systemctl status trading-bot
```

---

## 8. Troubleshooting

### Problema: Bot no se conecta a IOL

```plaintext
Síntoma: [ERROR] Fallo en autenticación a IOL

Solución:
1. Verificar credenciales en .env
   └─ ¿IOL_USERNAME es correcto e IOL_PASSWORD fue generado con `-e` en esta máquina?
   
2. Verificar que IOL API está disponible
   └─ curl https://api.invertironline.com/
   
3. Verificar token refresh
   └─ Si expiró, obtener uno nuevo manualmente
   
4. Revisar permisos de firewall
   └─ ¿Puerto 443 disponible?
```

### Problema: Posiciones no se cierran

```plaintext
Síntoma: Posición abierta > timeout sin vender

Solución:
1. Revisar logs: ¿error en API de venta?
2. Verificar disponibilidad de opción
3. Validar precio en IOL manualmente
4. Si es necesario: cierre manual en IOL
5. Registrar evento en el journal para auditoría (./data/journal/*.jsonl)
```

### Problema: Ganancia incorrecta

```plaintext
Síntoma: P&L no coincide con cálculos manuales

Solución:
1. Verificar comisión configurada (COMMISSION_PERCENTAGE)
2. Verificar impuesto (TAX_PERCENTAGE)
3. Revisar histórico en el journal:
   # Mostrar últimas operaciones (desde journal)
   tail -n 5 ./data/journal/*.jsonl | jq -c .
4. Comparar con IOL directamente
5. Reportar si hay diferencia consistente
```

---

## 9. Rollback a Versión Anterior

```bash
# Si nueva versión tiene bugs

# Ver historial
git log --oneline

# Volver a versión anterior
git checkout <commit-hash>

# Recompilar
cargo build --release

# Reiniciar bot
sudo systemctl restart trading-bot
```

---

## 10. Seguridad

### 10.1 Proteger .env

```bash
# Permisos restrictivos
chmod 600 .env
chmod 700 ./data && chmod 600 ./data/*.jsonl || true

# Verificar
ls -la .env ./data

# Debería mostrar: -rw------- solo para owner

# Verificar
ls -la .env

# Debería mostrar: -rw------- solo para owner
```

### 10.2 Encriptar Secretos

```bash
# Si usas 1Password, Bitwarden, etc:
# - Guardar IOL_REFRESH_TOKEN de forma encriptada
# - Usar variables de ambiente o secretos del SO

# En Linux, usar pass o similar:
pass insert iol/refresh-token
# Luego leer en script de deployment
```

### 10.3 Auditoría

```bash
# Todas las operaciones quedan en el journal (./data/journal/*.jsonl)
# Ejemplo: agrupar por día y mostrar métricas agregadas usando jq

# Concatenar todos los JSONL y agrupar por fecha (YYYY-MM-DD)
cat ./data/journal/*.jsonl | jq -s '
  map(.timestamp = (.timestamp // "") | .date = (.timestamp | split("T")[0]))
  | group_by(.date)
  | map({
      date: .[0].date,
      ops: length,
      comisiones_pagadas: (map(.commission_charged // 0) | add),
      impuestos_pagados: (map(.tax_charged // 0) | add),
      ganancia_neta: (map(.pnl_net // 0) | add)
    })'

# Resultado: lista JSON con campos date, ops, comisiones_pagadas, impuestos_pagados, ganancia_neta
```

---

**Guía de Deployment v1.0**
