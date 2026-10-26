# ADR 0012 — Preflight y plazos de arranque de MariaDB

- Estado: Aceptado
- Fecha: 2026-10-21
- Aceptado por el equipo: 2026-10-21

## Contexto

El [ADR 0003](0003-supervision-de-procesos.md) ya define health checks, la regla del LSN futuro y el reinicio con backoff. No fija cuánto esperar un arranque que no llega a «ready for connections», ni qué hacer con un datadir que arrastra archivos de replicación o un `multi-master.info` corrupto.

En instalaciones XAMPP se ha visto un datadir que no arranca porque `multi-master.info` contiene líneas de log en vez de nombres de conexión, junto con decenas de archivos de replicación huérfanos, y el error log se detiene en «Server socket created» sin llegar a aceptar conexiones. Hostig no configura replicación: cualquier archivo de ese tipo en una instancia creada por el producto es una anomalía. El detalle del caso reportado está en [../research/caso-xampp-74-multi-master.md](../research/caso-xampp-74-multi-master.md).

La importación desde XAMPP (fase 2 de la arquitectura) conecta al MariaDB de XAMPP que esté en marcha y no copia su datadir. Antes de esa conexión hace falta el mismo criterio de diagnóstico, en solo lectura, para no tratar como fuente fiable un datadir ya enfermo.

## Decisión

### Plazos de arranque

Tras lanzar MariaDB, el supervisor observa el tramo nuevo del error log y el `mariadb-admin ping` del [ADR 0003](0003-supervision-de-procesos.md).

1. **Cuelgue.** Si pasan N segundos sin ninguna línea nueva en el error log, sin la línea «ready for connections» (o el equivalente de esa versión) y sin respuesta al ping, el arranque falla. La causa cita la última línea vista del log, en la forma «colgado tras …». N vale 60 por defecto y se configura en el manifiesto global.
2. **Tope total.** Aunque el log siga avanzando (recuperación de InnoDB activa), el arranque no puede superar un tope total. El defecto es 15 minutos y también es configurable en el manifiesto global. Así una recuperación larga pero activa no se confunde con un cuelgue, y una recuperación interminable tampoco deja el `start` abierto para siempre.
3. **Misma última línea dos veces.** Si dos intentos seguidos se detienen en la misma última línea del log, no se reintenta dentro del backoff del [ADR 0003](0003-supervision-de-procesos.md). La instancia queda en error hasta que la persona actúe (por ejemplo, preflight, cuarentena o inspección del datadir).

Los valores N y el tope total viven en el manifiesto global, no en un conf editado a mano.

### Preflight antes de cada arranque

Antes de lanzar el proceso, el supervisor inspecciona el datadir de esa instancia (`{datos}/instances/{id}/`). Si el preflight falla, no se arranca.

#### `multi-master.info`

El archivo, si existe, debe estar vacío o contener solo nombres de conexión válidos: sin espacios, sin marcas de tiempo y sin marcas de log como `[Note]` o `[ERROR]`. Cualquier otro contenido es error. La acción es «poner en cuarentena».

#### Archivos de replicación huérfanos

`master-*.info`, `relay-log-*.info` y `mysql-relay-bin-*` que no correspondan a una conexión declarada en un `multi-master.info` válido se consideran huérfanos. Hostig no configura replicación: en una instancia creada por Hostig, cualquier archivo de replicación es una anomalía y se trata igual (error con acción de cuarentena).

#### Cuarentena

La cuarentena **mueve**, nunca borra, a:

```text
{datos}/instances/{id}/quarantine/{timestamp}/
```

Deja en esa carpeta un manifiesto de lo movido (rutas originales y destino) para poder deshacerlo con un comando del producto. No se ofrece borrado automático de lo cuarentenado.

#### Configuración regenerada

El [ADR 0005](0005-proyectos-y-plantillas.md) ya decide que el supervisor regenera la configuración y sustituye el archivo en disco si no coincide con la salida de las plantillas. Esta decisión no cambia eso. Añade solo que, al sustituirla, el supervisor:

- registra qué cambió (diff o lista de claves/directivas distintas);
- emite un aviso destacado si la versión editada a mano contenía `skip-grant-tables` o `skip-networking`.

### Importar desde XAMPP (fase 2)

Antes de conectar al MariaDB de XAMPP, el mismo preflight se ejecuta en **modo solo lectura** sobre el datadir de XAMPP y se lee su error log en busca, al menos, de: LSN in the future, tablespace no encontrado, `multi-master.info` corrupto y presencia de `skip-grant-tables`. Se muestra el diagnóstico. No se modifica nada de XAMPP sin confirmación explícita en ese momento. Si la persona confirma, la cuarentena sigue la misma regla (mover con manifiesto, nunca borrar).

Esto no sustituye la regla de la arquitectura: las bases se vuelcan conectando al servidor en marcha; no se copia el datadir de XAMPP como forma de importar datos.

### Criterios de aceptación (bloque MariaDB, fase 1)

Quedan escritos aquí; no se implementan en este ADR:

1. Un datadir con `multi-master.info` corrupto: el preflight lo detecta y la cuarentena se puede deshacer con el comando previsto.
2. Un arranque que no llega a ready: falla con la causa correcta («colgado tras …» u otra causa ya definida en el ADR 0003, según corresponda).
3. Una recuperación larga con el log avanzando: no se corta antes del tope total.

## Consecuencias

- El arranque de MariaDB tiene dos relojes distintos: silencio del log (cuelgue) y techo absoluto (recuperación interminable).
- Un datadir traído de fuera o tocado a mano puede quedar bloqueado hasta la cuarentena o la intervención de la persona. Eso es deliberado.
- La cuarentena ocupa disco. El manifiesto permite deshacer; no limpia solo.
- El aviso de `skip-grant-tables` / `skip-networking` no impide sustituir el conf (eso ya lo manda el ADR 0005); deja constancia de que la edición manual era peligrosa.
- El diagnóstico de importación de XAMPP no depende del módulo de IA ([ADR 0008](0008-diagnostico-ia.md)).

## Alternativas descartadas

- Tratar el silencio del log con el mismo reloj que la recuperación activa. Confunde un cuelgue temprano con un InnoDB lento.
- Borrar los archivos de replicación huérfanos. Impide deshacer y borra evidencia.
- Reintentar el backoff del ADR 0003 cuando dos arranques seguidos se detienen en la misma línea. Repite el mismo cuelgue sin aportar progreso.
- Copiar o «reparar» el datadir de XAMPP sin confirmación. Choca con la arquitectura y con la regla de no tocar software ajeno sin permiso explícito.
- Confiar el preflight al diagnóstico con IA. El producto tiene que detectar estos casos sin proveedor ni clave.
