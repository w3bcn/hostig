# ADR 0004 — Gestión de puertos

- Estado: Aceptado
- Fecha: 2026-10-20

## Contexto

XAMPP comprueba puertos con `GetExtendedTcpTable` y puede negarse a arrancar, pero sus scripts de parada no identifican al dueño: matan por nombre de imagen. En la inspección, el único `mysqld` vivo era de otra instalación y escuchaba en 3307, mientras esta raíz declaraba 3306 en tres archivos distintos y un cuarto `my.ini` apuntaba a otro directorio de datos.

Hostig necesita saber, antes de crear un proceso, si el puerto está libre y, si no lo está, quién lo tiene. El módulo de puertos no mata procesos. Informa al supervisor.

## Decisión

`ports` es un módulo con una interfaz pequeña: `check(puerto, pid_esperado?) -> libre | ocupado { pid, ruta del ejecutable, nombre } | ocupado_dueño_no_visible | reservado` y `suggest(preferido, rango) -> puerto libre`. Si el ocupante es el `pid_esperado`, no hay conflicto.

### Detección

Se consulta la tabla TCP del sistema, no se lanza `netstat` como proceso.

- Windows: `GetExtendedTcpTable` con dueño de PID (`TCP_TABLE_OWNER_PID_LISTENER`), IPv4 e IPv6. El supervisor es un binario de 64 bits, para no repetir el panel de 32 bits de XAMPP preguntando por servidores de 64 bits. La ruta del ejecutable sale de `QueryFullProcessImageNameW`.
- Linux: se lee `/proc/net/tcp` y `/proc/net/tcp6`, y el PID se resuelve por los inodos en `/proc/*/fd`.
- macOS: `proc_pidfdinfo` / las APIs de libproc sobre la tabla de sockets. Si en la implementación una llamada exige privilegios para ver procesos de otros usuarios, el resultado es «ocupado, dueño no visible» y se trata como conflicto. No se asume que está libre.

Solo interesan listeners en `0.0.0.0`, `::` o loopback. Una conexión saliente no bloquea el puerto local de escucha.

### Antes de arrancar

El supervisor pide `check` para cada puerto del manifiesto (HTTP, HTTPS si aplica, MariaDB, y en Windows el puerto de cada `php-cgi`). Si el ocupante es el PID que este supervisor ya registra para ese servicio, no hay conflicto. Si es cualquier otro proceso, el arranque no empieza.

La respuesta al usuario incluye el PID, el nombre y la ruta cuando el sistema las da, y un puerto alternativo: el primer libre desde el preferido, dentro de un rango (HTTP 8080–8099, HTTPS 8443–8459, MariaDB 3306–3399, PHP en Windows 9000–9099). El cambio de puerto no se aplica solo. Hace falta una confirmación, que actualiza el manifiesto y regenera la configuración ([ADR 0005](0005-proyectos-y-plantillas.md)).

No se ofrece «cerrar el proceso que molesta» en el MVP. Un botón así, sobre todo si el dueño es otra copia de MariaDB, repite el fallo de `taskkill` por nombre.

### Puertos bajos

80, 443 y 53 no se usan en el modo sin privilegios ([ADR 0006](0006-privilegios.md)). El módulo los rechaza con la acción «usar el modo con helper, o un puerto alto».

### Puertos reservados por el sistema

En Windows, Hyper-V, WSL y Docker reservan rangos de puertos (los que muestra `netsh int ipv4 show excludedportrange`). En esos rangos no hay ningún proceso escuchando, pero `bind` falla con `WSAEACCES` (10013).

Cuando la tabla TCP diga que el puerto está libre, Hostig hace una prueba de `bind` en `127.0.0.1` y suelta el socket de inmediato. Si falla con acceso denegado, el resultado es **Reservado**, con la causa «puerto reservado por el sistema (Hyper-V, WSL o Docker)» y la acción de usar el puerto sugerido. `suggest` también salta los puertos reservados.

En Linux y macOS el mismo criterio aplica si `bind` devuelve `EACCES`/`EPERM` sin un listener visible.

## Consecuencias

- Arrancar con el puerto ocupado falla rápido y con una acción clara, en vez de dejar a Apache en un reinicio con backoff.
- Identificar el ejecutable de un PID de otro usuario puede fallar. El conflicto se mantiene; solo falta el nombre.
- Los rangos son convencionales. Si están llenos, `suggest` devuelve error y la persona elige un puerto a mano en el manifiesto.

## Alternativas descartadas

- Intentar `bind` y traducir el error del sistema. Dice que está ocupado y no dice quién.
- `netstat` o `lsof` como dependencia externa. Es otro proceso y otro formato que parsear.
- Matar al ocupante desde este módulo. Viola el límite del supervisor y el de los privilegios.
