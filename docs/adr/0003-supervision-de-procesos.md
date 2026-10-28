# ADR 0003 — Supervisión de procesos

- Estado: Aceptado
- Fecha: 2026-10-20

## Contexto

El análisis de XAMPP en `C:\xampp_8_2` dejó hechos que este módulo tiene que evitar ([xampp-analisis.md](../research/xampp-analisis.md)):

- `mysql_stop.bat` llama a `taskkill /F` por el nombre `mysqld.exe`, con una ruta `@@BITROCK_INSTALLDIR@@` sin sustituir, y borra `%COMPUTERNAME%.pid` cuando el archivo real es `mysql.pid`.
- Esos PID files apuntaban a procesos que ya no existían.
- El panel no contiene `mysqladmin`. Importa `TerminateProcess`. El registro de MariaDB encadena arranques con `Starting crash recovery` y sin `Shutdown complete`.
- InnoDB llegó a anotar páginas con LSN por delante del redo («may be corrupt» o tablespace copiado sin sus logs) y aun así escribió que el servidor había arrancado.
- En la misma máquina había otro `mysqld` de otra instalación. Matar por nombre de imagen los habría confundido.
- `innodb_flush_log_at_trx_commit=1` y el doublewrite protegen un commit si el redo y el tablespace siguen siendo la misma pareja. No convierten un kill en un apagado.

El módulo de procesos es el único que crea y destruye procesos de servidores. La UI no mata PID ni edita `my.ini`.

## Decisión

Cada servidor en marcha es un registro del supervisor: identificador, ruta del ejecutable, argumentos, `--defaults-file` o equivalente, PID, manejador de proceso, puertos y directorio de datos. El PID file que escriba Apache o MariaDB no es la fuente de verdad. Solo se acepta si el proceso sigue vivo y su línea de comandos pertenece a esta carpeta de datos.

### Arranque

Orden: MariaDB, PHP (el pool del proyecto o el pool único del MVP), Apache. Apache no pasa a «listo» hasta que su health check responde. PHP no se considera listo hasta que el socket o el puerto del pool acepta conexiones.

MariaDB se inicializa la primera vez con `mariadb-install-db` sobre un datadir vacío dentro de `{datos}/instances/{id}/`. Se genera una contraseña de la cuenta de administración y se guarda con permisos de usuario (archivo `0600` en el directorio de la instancia; en una fase posterior, el llavero del sistema). No se reutiliza la raíz sin contraseña de XAMPP.

En Windows, `mariadb-install-db` recibe esa contraseña por línea de comandos (`--password=...`) solo durante la inicialización. Queda visible unos segundos para el mismo usuario y para administradores que listen el proceso; no es el camino de `mariadb-admin` (que usa un archivo de opciones protegido). En Linux cualquier usuario puede leer las líneas de comandos de otros procesos, así que la fase 1b necesitará otro mecanismo (p. ej. opciones en archivo o entrada sin argv).

El proceso se lanza con el ejecutable absoluto del runtime y un archivo de configuración generado. No se hereda un `PATH` modificado del sistema.

### Health checks

| Proceso | Comprobación | Intervalo inicial |
|---|---|---|
| MariaDB | `mariadb-admin --protocol=tcp ping` contra `127.0.0.1` y el puerto de esa instancia, con el usuario de administración | 2 s en el arranque, luego 5 s |
| PHP | Conexión al socket del pool (Unix) o al `127.0.0.1` de `php-cgi` (Windows) | Igual |
| Apache | `GET /__hostig/health` en el puerto de esa instancia, respuesta 200 generada por nosotros | Igual |

Un health check fallido no mata al momento. Tres fallos seguidos marcan el proceso como caído.

Después de cada arranque de MariaDB el supervisor lee el tramo nuevo de su error log. `Starting crash recovery` tras un cierre anterior es recuperación normal de InnoDB y se registra como aviso. Cualquiera de estas líneas es fallo, aunque el puerto ya acepte conexiones: `log sequence number is in the future`, `may be corrupt`, o un volcado nuevo (`mysqld.dmp` o equivalente) aparecido durante ese arranque. La acción del error es no usar esa instancia para proyectos hasta que la persona decida, y no lanzar una recuperación forzada automática.

### Reinicio con backoff

Si el proceso muere solo (no porque el usuario pidió parar), el supervisor lo reinicia con esperas de 1 s, 2 s, 4 s, 8 s y 16 s, como máximo cinco veces en diez minutos. Agotado el cupo, queda en error y no se reintenta hasta un `start` explícito. Cada intento escribe causa y acción.

Salir con código 0 al cumplir `PHP_FCGI_MAX_REQUESTS` es un reciclado del pool de Windows, no una caída. No gasta este cupo. Lo concreta el [ADR 0005](0005-proyectos-y-plantillas.md).


Apache se reinicia sin tocar MariaDB. PHP se reinicia sin tocar MariaDB. Si MariaDB está en error, Apache puede seguir sirviendo estáticos, y los proyectos PHP pasan a error con la causa «la base no está sana».

### Apagado ordenado

Orden inverso: Apache (deja de aceptar y termina sus hijos), pools PHP, MariaDB.

MariaDB se apaga con `mariadb-admin shutdown` al host y puerto de **esa** instancia. El supervisor espera la salida del proceso con un plazo (30 s en el MVP). El éxito exige las tres cosas: el proceso ha salido, el puerto está libre y el error log contiene la línea de apagado del servidor. Si el plazo vence, se registra el error. No hay `taskkill /F` ni `TerminateProcess` en el camino normal.

Solo un comando explícito de emergencia, distinto de `stop`, puede terminar el PID que el supervisor tiene registrado. Debe citar el PID y la ruta, nunca el nombre de imagen. Queda en el log como cierre brusco. El arranque siguiente espera recuperación de InnoDB y aplica la regla del LSN.

Apache en Unix recibe `SIGWINCH` y luego espera; si no sale, `SIGTERM`. En Windows (bloque 4, ZIP Apache Lounge VS18): la vía elegida es **`TerminateProcess` únicamente del PID guardado en el `ProcessRecord`** (y el `Child` del supervisor), nunca por nombre de imagen (constante `WINDOWS_SHUTDOWN_METHOD`). Prueba 2026-10-28: (1) `httpd -k shutdown` no detuvo el proceso lanzado con `-f` — el interruptor del Lounge apunta al servicio Windows `Apache2.4`; (2) ctrl-break al grupo con `GenerateConsoleCtrlEvent` tampoco — el arranque usa `CREATE_NO_WINDOW` ([ADR 0011](0011-apagado-del-sistema.md)) y ese httpd no tiene consola que reciba el evento. A diferencia de MariaDB, Apache no mantiene un datadir InnoDB: cortar el PID registrado basta para liberar el puerto. Queda descartado `taskkill` por nombre.

### Cierre brusco del propio supervisor

Al arrancar, el supervisor lee su registro. Para cada servicio que figure en marcha:

- si el PID vive y la línea de comandos coincide, lo adopta y sigue supervisando;
- si el PID no vive, lo marca caído, no borra el datadir y ofrece `start`, que en MariaDB pasará por recuperación;
- si el PID vive pero la ruta no es nuestra, no lo toca y lo reporta como ajeno.

En Windows el supervisor no mete a MariaDB en un Job Object con `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. Si el supervisor muere, la base tiene que seguir viva para poder adoptarla. El apagado del sistema, el cierre de sesión y la suspensión no hacen volcado y usan plazos más cortos: [ADR 0011](0011-apagado-del-sistema.md). La espera de 30 segundos de arriba es la del `stop` pedido por la persona, no la del apagado del sistema.

## Consecuencias

- Parar es más lento que un kill, y es la medida correcta.
- Hace falta un test de proceso: arrancar, pedir stop, afirmar puerto libre y línea de shutdown en el log. Es el criterio de terminado del módulo.
- El reinicio automático puede ocultar un fallo de configuración. El cupo de cinco intentos y el error estable lo limitan.
- La emergencia que mata un PID es una vía real de corrupción. Existe para un proceso colgado, queda auditada y no es el botón de parar.

## Alternativas descartadas

- Servicios de Windows o launchd como modelo por defecto. Piden privilegios y mezclan el producto con el sistema. El supervisor de usuario cubre el MVP.
- Confiar en el PID file del servidor.
- Considerar «el puerto escucha» como sinónimo de «InnoDB está sano».
