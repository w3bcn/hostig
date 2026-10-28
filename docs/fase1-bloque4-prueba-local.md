# Prueba real bloque 4 (Windows)

`HOSTIG_DATA_DIR` temporal (`%TEMP%\hostig-php-apache-demo`). Runtimes del catálogo: MariaDB 11.4.13, PHP 8.4.27 NTS, Apache Lounge 2.4.69.

## Comandos

```text
hostig supervisor start
hostig start --http-port 8080 --db-port 3310
hostig status
# GET http://127.0.0.1:8080/  → 200 (HTML abajo / fase1-bloque4-index.html)
# GET http://127.0.0.1:8080/__hostig/health → 200 ok
hostig stop
```

## hostig status (tras start)

```json
{
  "apache": {"error": null, "id": "apache-default", "pid": 13184, "port": 8080, "state": "running", "version": "2.4.69"},
  "mariadb": {"error": null, "hang_blocked": false, "id": "mariadb-11.4", "pid": 38344, "port": 3310, "state": "running", "version": "11.4.13"},
  "php": {
    "crash_restarts": 0,
    "error": null,
    "id": "php-default",
    "state": "running",
    "version": "8.4.27",
    "workers": [
      {"id": "php-default-0", "pid": 29300, "port": 9000},
      {"id": "php-default-1", "pid": 36820, "port": 9001},
      {"id": "php-default-2", "pid": 3188, "port": 9002},
      {"id": "php-default-3", "pid": 34508, "port": 9003}
    ]
  }
}
```

## Health

`HTTP 200` cuerpo `ok`.

## Apagado Apache (decisión)

Tras probar `httpd -k shutdown` y ctrl-break con `CREATE_NO_WINDOW`, el stop usa `TerminateProcess` del PID del registro (ADR 0003). `hostig stop` terminó con exit 0 y puertos 8080/3310/9000 libres.
