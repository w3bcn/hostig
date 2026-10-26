# Decisiones de arquitectura

Cada archivo es una decisión. El mapa de módulos, la estructura del repositorio y el roadmap están en [../ARCHITECTURE.md](../ARCHITECTURE.md).

| ADR | Estado | Decisión |
|---|---|---|
| [0001](0001-supervisor-ipc.md) | Aceptado | El supervisor es un proceso aparte. UI y CLI hablan con él por un socket local. |
| [0002](0002-gestor-de-runtimes.md) | Aceptado | Descarga bajo demanda, checksum fijado, builds de CI por fases y DLL de Visual C++ junto al runtime. |
| [0003](0003-supervision-de-procesos.md) | Aceptado | Health checks, reinicio con backoff y apagado ordenado de MariaDB. |
| [0004](0004-gestion-de-puertos.md) | Aceptado | Antes de arrancar se identifica al dueño del puerto y se propone otro. |
| [0005](0005-proyectos-y-plantillas.md) | Aceptado | MariaDB compartida por versión mayor. En Windows, un pool de N `php-cgi`. |
| [0006](0006-privilegios.md) | Aceptado | Helper elevado mínimo, y un modo que no lo necesita. |
| [0007](0007-backups.md) | Aceptado | Volcado solo si hubo escrituras o la copia tiene más de 24 h. Si hace falta y falla, no se para. Nunca durante el apagado del sistema. |
| [0008](0008-diagnostico-ia.md) | Aceptado | IA opcional. El producto funciona entero sin ella. |
| [0009](0009-licencia.md) | Aceptado | Apache-2.0, con `LICENSE` y `NOTICE`. |
| [0010](0010-distribucion.md) | Aceptado | CI en los tres sistemas desde el principio. Firma de Windows primero. Los runtimes no van dentro del instalador. |
| [0011](0011-apagado-del-sistema.md) | Aceptado | Aviso de apagado, cierre de sesión y suspensión. Parada rápida sin volcado. MariaDB no muere si muere el supervisor. |
| [0012](0012-preflight-mariadb.md) | Aceptado | Preflight del datadir, cuarentena de replicación anómala y plazos de arranque de MariaDB (cuelgue vs recuperación activa). |


Las cinco decisiones que estaban abiertas quedan confirmadas al final de la arquitectura. El límite de identidad de Azure Trusted Signing está en el [ADR 0010](0010-distribucion.md).
