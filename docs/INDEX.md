# Índice de Documentación - Trading Automático de Opciones

**Documentación generada:** Agosto 2026  
**Total:** 2,648 líneas en documentación | ~80 KB  
**Lenguaje:** Rust + Tokio (persistencia en memoria + journal por defecto)

---

## 📑 Tabla de Contenidos

### 1. [**README.md** (304 líneas)](../README.md) ⭐ COMIENZA AQUÍ
**Punto de entrada general**

**Secciones:**
- 📋 Documentos de arquitectura (overview)
- 🎯 Características principales
- 📐 Arquitectura simplificada
- 🚀 Quick start en 4 pasos
- 📊 Configuración esencial
- 🏗️ Estructura del proyecto
- 🔒 Seguridad
- 📈 Performance
- 🧪 Testing
- ⚠️ Disclaimers
- 📞 Soporte

**Para quién:** Todos (inicio rápido)  
**Lectura:** 5 minutos

---

### 2. [**EXECUTIVE_SUMMARY.md** (346 líneas)](EXECUTIVE_SUMMARY.md) 📊 PARA LÍDERES
**Resumen ejecutivo con decisiones y KPIs**

**Secciones:**
- 🎯 Propósito del proyecto
- 📊 Matriz de decisiones (Rust, Tokio, persistencia in-memory)
- 🏛️ Patrones arquitectónicos clave
- 💰 Fórmulas de negocio (P&L, threshold de venta)
- ⏱️ Parámetros operacionales recomendados
- 🔐 Seguridad de credenciales
- 📊 Performance targets vs actual
- 🛡️ Resiliencia (RTO por scenario)
- 📈 Fases de implementación (~8 semanas)
- 🎯 KPIs sugeridos
- 📋 Checklist pre-producción
- 🚨 Limitaciones conocidas
- 🔮 Roadmap futuro
- 💡 Key insights
- 🏁 Conclusión

**Para quién:** Tech leads, Managers, Arquitectos  
**Lectura:** 10 minutos

---

### 3. [**ARCHITECTURE.md** (578 líneas)](ARCHITECTURE.md) 🏗️ DISEÑO TÉCNICO COMPLETO
**Documento técnico principal - Arquitectura general**

**Secciones:**

#### 3.1 Visión General
- Objetivos del proyecto
- Stack tecnológico

#### 3.2 Arquitectura General
- Diagrama de componentes
- Relaciones entre módulos

#### 3.3 Componentes Principales
- **3.3.1 Módulo de Configuración**
  - Variables de ambiente
  - Validación en startup
  - Fallback a defaults
  
- **3.3.2 Módulo de Datos de Mercado**
  - IOL API Client
  - Price Stream Manager
  - Cache Layer
  
- **3.3.3 Módulo Detector de Patrones**
  - Algoritmo de tendencia
  - Estadísticas calculadas
  
- **3.3.4 Módulo Motor de Trading**
  - Máquina de estados
  - Position Manager
  - Cálculo de P&L
  
- **3.3.5 Módulo Portafolio**
  - Seguimiento de posiciones
  - Histórico de operaciones
  
- **3.3.6 Módulo Persistencia**
  - Estrategia de almacenamiento
  - Cache TTL y validación de datos

#### 3.4 Flujos de Negocio
- Detección de tendencia y ejecución
- Ciclo de obtención de precio

#### 3.5 Requisitos No Funcionales
- Performance (latencias, memoria, CPU)
- Estabilidad (reconexión, circuit breaker)
- Confiabilidad (idempotencia, auditoría)

#### 3.6 Seguridad
- Credenciales y tokens OAuth
- HTTPS y validación de certificados
- Enmascaramiento de datos sensibles

#### 3.7 Estructura de Directorios
- Layout propuesto del proyecto

#### 3.8 Dependencias Principales
- Stack de librerías Rust recomendadas

#### 3.9 Decisiones Arquitectónicas
- Justificación de cada elección tecnológica

#### 3.10 Plano de Implementación
- 6 fases desde fundamentos a deployment

#### 3.11 Pruebas
- Estrategia unitaria, integración, E2E

#### 3.12 Monitoreo y Observabilidad
- Métricas clave
- Formato de logs

#### 3.13 Escalabilidad Futura
- Caminos para expansión

#### 3.14 Apéndice: Fórmulas Clave
- Detección de tendencia
- Cálculo de P&L

**Para quién:** Arquitectos, Senior Developers  
**Lectura:** 30 minutos  
**Uso:** Referencia durante diseño e implementación

---

### 4. [**IMPLEMENTATION_DETAILS.md** (593 líneas)](IMPLEMENTATION_DETAILS.md) 🔧 ESPECIFICACIÓN GRANULAR
**Detalles técnicos de implementación - Para Developers**

**Secciones:**

#### 4.1 Módulo de Configuración
- Validación en startup (pseudocódigo)
- Valores por defecto sugeridos

#### 4.2 Autenticación OAuth 2.0 con IOL
- Flujo inicial de setup
- Refresh automático

#### 4.3 Gestión de Datos de Precio
- Estructura del buffer histórico
- Validación de datos

#### 4.4 Lógica de Detección de Tendencia
- Algoritmo detallado (pseudocódigo)
- Detección de cambio de tendencia
- Métricas de fuerza (R², volatilidad)

#### 4.5 Motor de Trading - Máquina de Estados
- Estados y transiciones detalladas
- Condiciones de transición (A, B, C, D)
- Detalles de P&L

#### 4.6 Gestión de Órdenes
- Ciclo de compra (6 pasos)
- Ciclo de venta (5 pasos)

#### 4.7 Persistencia - Estructuras en memoria
- operations: Vec/structs en memoria
- positions: DashMap/HashMap
- journal: JSONL append-only (./data/journal)
- snapshots: JSON.gz periódicos (./data/snapshots)

#### 4.8 Recuperación ante Fallos
- Snapshots del estado
- Journal de operaciones

#### 4.9 Manejo de Errores y Resiliencia
- Errores de red (tabla de respuesta)
- Errores de lógica (tabla de decisión)

#### 4.10 Optimizaciones de Performance
- Caché de strikes
- Batch de cálculos
- Memoria acotada

#### 4.11 Testing
- Mock de IOL API
- Escenarios de tendencia

---

**Para quién:** Developers, QA  
**Lectura:** 45 minutos  
**Uso:** Guía de implementación línea por línea

---

### 5. [**DEPLOYMENT.md** (680 líneas)](DEPLOYMENT.md) 🚀 GUÍA OPERACIONAL
**Instalación, configuración y operación en producción**

**Secciones:**

#### 5.1 Preparación del Ambiente
- Requisitos previos
- Instalación inicial
- Estructura de .env

#### 5.2 Inicialización de Persistencia
- Preparar ./data/journal y ./data/snapshots
- Verificar permisos y espacio en disco

#### 5.3 Validación Pre-Lanzamiento
- Checklist de configuración
- Test de conexión a IOL
- Test de persistencia (escritura en journal/snapshots)

#### 5.4 Ejecución Local
- Modo debug
- Modo release
- Parar el bot correctamente

#### 5.5 Deployment en Producción
- **Opción A: Servidor Linux**
  - Preparación del servidor
  - Deploy de código
  - Configuración systemd (recomendado)
  - Alternativa: tmux
  
- **Opción B: Docker**
  - Dockerfile
  - Build y run
  - Docker Compose (recomendado)

#### 5.6 Monitoreo en Producción
- Análisis de logs
- Métricas diarias desde journal JSONL
- Alertas recomendadas (scripts)

#### 5.7 Mantenimiento
- Rotación de logs (logrotate)
- Backup de datos (diario)
- Actualización de bot

#### 5.8 Troubleshooting
- Problema: No conecta a IOL
- Problema: Posiciones no se cierran
- Problema: Ganancia incorrecta

#### 5.9 Rollback
- Volver a versión anterior

#### 5.10 Seguridad
- Proteger .env
- Encriptar secretos
- Auditoría de operaciones

**Para quién:** DevOps, SRE, Operators  
**Lectura:** 45 minutos  
**Uso:** Procedimientos operacionales día a día

---

## 🎯 Guía de Lectura por Rol

### 👨‍💼 Managers / Stakeholders
1. Comienza con **README.md** (5 min)
2. Lee **EXECUTIVE_SUMMARY.md** (10 min)
3. Opcional: Revisa fases en **ARCHITECTURE.md** sección 3.10
**Total:** 15 minutos

### 🏗️ Arquitectos / Tech Leads
1. Lee **EXECUTIVE_SUMMARY.md** (10 min)
2. Estudia **ARCHITECTURE.md** completo (30 min)
3. Revisa decisiones en sección 3.9
4. Opcional: Valida con **IMPLEMENTATION_DETAILS.md** secciones clave
**Total:** 45 minutos

### 👨‍💻 Developers (Frontend)
1. Lee **README.md** (5 min)
2. Consulta **IMPLEMENTATION_DETAILS.md** secciones relevantes (30 min)
3. Usa **DEPLOYMENT.md** para setup local (15 min)
**Total:** 50 minutos

### 👨‍💻 Developers (Backend)
1. Lee **ARCHITECTURE.md** completo (30 min)
2. Estudia **IMPLEMENTATION_DETAILS.md** completo (45 min)
3. Setup con **DEPLOYMENT.md** (15 min)
4. Usa **EXECUTIVE_SUMMARY.md** para contexto de negocio (10 min)
**Total:** 100 minutos

### 🔧 DevOps / SRE
1. Comienza con **DEPLOYMENT.md** sección 5.1 (10 min)
2. Ejecuta checklist sección 5.3 (10 min)
3. Configura systemd o Docker sección 5.5 (20 min)
4. Revisa monitoreo sección 5.6 (15 min)
5. Lee troubleshooting sección 5.8 (10 min)
**Total:** 65 minutos

### 🧪 QA / Testers
1. Lee **README.md** sección testing (5 min)
2. Revisa **ARCHITECTURE.md** sección 3.11 (10 min)
3. Estudia **IMPLEMENTATION_DETAILS.md** sección 4.11 (15 min)
4. Setup local en **DEPLOYMENT.md** (15 min)
**Total:** 45 minutos

---

## 🔗 Referencias Cruzadas

### Conceptos relacionados

| Concepto | Ubicación | Secciones |
|----------|-----------|----------|
| **Máquina de estados** | ARCHITECTURE (3.4.1) + IMPL (4.5) | Diagrama + Detalles |
| **P&L Calculation** | ARCHITECTURE (3.4.2) + IMPL (4.5.2) | Fórmula + Implementación |
| **OAuth Flow** | IMPL (4.2) | Detallado |
| **Buffer de precios** | ARCHITECTURE (3.2.2) + IMPL (4.3) | Concepto + Estructura |
| **Detección tendencia** | ARCHITECTURE (3.3.3) + IMPL (4.4) | Algoritmo + Pseudocódigo |
| **Seguridad** | ARCHITECTURE (3.6) + DEPLOYMENT (5.10) | Diseño + Operacional |
| **Recuperación fallos** | IMPL (4.8) + DEPLOYMENT (5.8) | Técnica + Troubleshooting |

---

## ⏱️ Timeline de Lectura Recomendada

### Día 1: Entendimiento General (1 hora)
```
├─ README.md (5 min)
├─ EXECUTIVE_SUMMARY.md (15 min)
└─ Primer vistazo ARCHITECTURE.md (40 min)
```

### Día 2: Deep Dive Arquitectura (2 horas)
```
├─ ARCHITECTURE.md completo (60 min)
└─ IMPLEMENTATION_DETAILS.md overview (60 min)
```

### Día 3: Implementación (2 horas)
```
├─ IMPLEMENTATION_DETAILS.md completo (70 min)
├─ DEPLOYMENT.md sección setup (50 min)
└─ Preguntas + clarificaciones (0 min)
```

### Día 4: Operación (1.5 horas)
```
├─ DEPLOYMENT.md completo (60 min)
└─ Troubleshooting + Checklist (30 min)
```

**Total:** ~6.5 horas para dominio completo

---

## 📊 Métricas de Documentación

| Métrica | Valor |
|---------|-------|
| Total de líneas | 2,648 |
| Total de secciones | 50+ |
| Diagramas ASCII/Mermaid | 20+ |
| Pseudocódigo/Ejemplos | 30+ |
| Tablas de referencia | 25+ |
| Fórmulas matemáticas | 15+ |
| Checklists | 10+ |
| Scripts SQL | 5+ |
| Ejemplos bash | 15+ |
| Configuraciones | Docker, systemd, env |

---

## 🎓 Cómo Usar Esta Documentación

### Para Diseño
1. Leer ARCHITECTURE.md completo
2. Validar decisiones en EXECUTIVE_SUMMARY.md
3. Consultar secciones específicas en IMPLEMENTATION_DETAILS.md

### Para Implementación
1. Usar IMPLEMENTATION_DETAILS.md como guía
2. Consultar ARCHITECTURE.md para contexto
3. Hacer preguntas en función del pseudocódigo

### Para Deployment
1. Seguir DEPLOYMENT.md paso a paso
2. Usar checklists de validación
3. Ejecutar scripts de inicialización

### Para Operación
1. Revisar DEPLOYMENT.md sección 5.6 (Monitoreo)
2. Usar `jq` sobre el journal JSONL para métricas
3. Consultar sección 5.8 (Troubleshooting)

### Para Testing
1. Revisar ARCHITECTURE.md sección 3.11
2. Usar mocks descritos en IMPLEMENTATION_DETAILS.md
3. Implementar escenarios de tendencia

---

## ✅ Validación de Cobertura

- [x] Arquitectura general explicada
- [x] Componentes detallados
- [x] Flujos de negocio documentados
- [x] Algoritmos incluidos
- [x] Schema de persistencia (operations/positions/journal) documentado
- [x] Seguridad abordada
- [x] Performance especificada
- [x] Deployment cubierto
- [x] Troubleshooting incluido
- [x] Ejemplos proporcionados
- [x] Diagramas incluidos

---

## 📞 Guía de Consulta Rápida

**¿Cómo...?**
- ...obtengo precios? → IMPL 4.3
- ...detecta tendencias? → IMPL 4.4
- ...compro/vendo? → IMPL 4.6
- ...recupero después de crash? → IMPL 4.8
- ...encuentro un error? → DEPLOYMENT 5.8
- ...configuro el bot? → DEPLOYMENT 5.1
- ...monitoreo en producción? → DEPLOYMENT 5.6
- ...calculo ganancia? → ARCHITECTURE 3.4.2 o EXECUTIVE 5
- ...aseguro credenciales? → ARCHITECTURE 3.6 o DEPLOYMENT 5.10

**¿Qué es...?**
- ...la máquina de estados? → ARCHITECTURE 3.4.1
- ...el buffer de precios? → IMPL 4.3
- ...un snapshot? → IMPL 4.8
- ...el P&L? → ARCHITECTURE 3.4.2
- ...circuit breaker? → ARCHITECTURE 3.5.1

---

**Documentación completada:** Agosto 2026  
**Mantenida por:** Equipo de Arquitectura  
**Próxima revisión:** Diciembre 2026

