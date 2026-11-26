# Fase 1 · Bloque 5b — Apagado del sistema (ADR 0011)

## Qué hace

| Aviso | Windows | Linux / macOS | Resultado |
|---|---|---|---|
| Apagado | `WM_QUERYENDSESSION` + `WM_ENDSESSION` en una ventana de nivel superior oculta (clase `HostigSupervisorShutdownWindow`) | `SIGTERM` | Parada rápida |
| Cierre de sesión | El mismo par con `ENDSESSION_LOGOFF`, más `WTS_SESSION_LOGOFF` (`WTSRegisterSessionNotification`) | `SIGTERM` | Parada rápida |
| Suspensión / reanudación | `RegisterSuspendResumeNotification` con `DEVICE_NOTIFY_CALLBACK` (y `WM_POWERBROADCAST` en la ventana, al mismo manejador) | — | No se para nada; queda en el log el estado al suspender y al reanudar |

Parada rápida (`StackService::system_shutdown_stop`):

- Orden Apache → PHP → MariaDB. Apache y PHP tienen como mucho 3 s cada uno. Si no paran a tiempo, se sigue igual.
- MariaDB se para con `StopOptions { system_shutdown: true }`: no hay `mariadb-dump`, se usa `mariadb-admin shutdown` y no hay `TerminateProcess`.
- Objetivo de unos 5 s y tope duro de 20 s para todo el camino. Si vence el tope, se registra el error y la parada ordenada de MariaDB sigue en segundo plano.
- En Windows es síncrona dentro de `WM_ENDSESSION`: MariaDB está parada antes de que el mensaje vuelva. Mientras dura, `ShutdownBlockReasonCreate` enseña «Hostig está deteniendo MariaDB de forma ordenada».
- El log `hostig.log` (módulo `apagado`) registra el tiempo de cada servicio y el total.
- Desde que empieza, `db.start` y `stack.start` se rechazan. Al terminar, el supervisor sale.

## Tests automáticos

`apps/cli/tests/apagado_sistema.rs`, con el falso mariadbd:

- `apagado_del_sistema_para_mariadb_sin_volcado_y_a_tiempo`: `SIGTERM` en Unix, o `WM_QUERYENDSESSION` + `WM_ENDSESSION` en Windows. Comprueba cuatro cosas:
  - MariaDB parada antes de que vuelva `WM_ENDSESSION`, en Windows;
  - la línea de apagado en el error log, ninguna copia nueva y el puerto libre;
  - que todo dure menos de 20 s;
  - que el supervisor salga.
- `suspension_y_reanudacion_no_paran_nada`, solo en Windows: tras `PBT_APMSUSPEND` y `PBT_APMRESUMEAUTOMATIC`, MariaDB sigue con el mismo PID y no se ha volcado nada.

## Pendiente para el bloque 6 (medición manual en Windows Sandbox)

Son las dos verificaciones de fase 1 del ADR 0011. No se pueden automatizar en CI. El resultado se apunta en el informe de la fase.

1. **Margen real tras `WM_ENDSESSION`.**
   1. Arranca el stack con datos (`hostig start`, y crea una tabla con escrituras).
   2. Cierra la sesión y, en otra prueba, apaga el Sandbox.
   3. En el arranque siguiente, lee `hostig.log`. Tiene que aparecer «parada rápida terminada en N ms», con los tiempos de cada servicio.
   4. En `error.log` de MariaDB tiene que aparecer «Shutdown complete» y, en el arranque siguiente, ninguna recuperación de InnoDB.
   5. Repite con el stack recién arrancado, con una escritura larga en curso y con el equipo cargado.
   6. Apunta N y si Windows llegó a cortar el proceso.
2. **`CTRL_SHUTDOWN_EVENT` / `CTRL_LOGOFF_EVENT` en `mariadbd.exe` y `httpd.exe`.** Para cada binario y cada evento, anota si sale, si hace su propio shutdown, si lo ignora o si el evento no llega.

Hasta tener esas medidas no se acorta el objetivo de 5 s ni el tope de 20 s.

## Fuera de este bloque

- Linux: inhibidor de retardo de logind (`PrepareForShutdown` / `PrepareForSleep`).
- macOS: `IORegisterForSystemPower` y las notificaciones de `NSWorkspace`.

Hoy, en esos dos sistemas, solo `SIGTERM` lleva a la parada rápida.
