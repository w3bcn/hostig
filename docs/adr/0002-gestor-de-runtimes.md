# ADR 0002 — Gestor de runtimes

- Estado: Aceptado. Los builds de CI desde el fuente oficial se aceptan por fases: Windows en la fase 1, Linux en la 1b y macOS en la 1c.
- Fecha: 2026-10-20

## Contexto

Hostig no debe copiar el modelo de XAMPP: un árbol fijo en `C:\` con binarios viejos, un `my.ini` dentro de `data` que apunta a otra ruta, y el runtime mezclado con el código del producto. Las prioridades son estabilidad y luego eficiencia. Varias versiones tienen que poder convivir. Nada se instala en `Program Files`, `/usr` ni `/opt`, y no se toca el `PATH` del sistema.

El instalador de Hostig no redistribuye Apache, PHP ni MariaDB ([ADR 0009](0009-licencia.md)). El supervisor los descarga cuando hacen falta.

Investigación de fuentes, octubre de 2026:

| Componente | Qué publica el upstream | ¿Archivo reubicable en la carpeta del usuario? |
|---|---|---|
| Apache HTTP Server | La ASF publica **solo código fuente**, con SHA-256, SHA-512 y firma PGP. Página: <https://httpd.apache.org/download.cgi>. Verificación: <https://httpd.apache.org/dev/verification.html>. Versión estable citada entonces: 2.4.69 (2026-10-01). | Windows: la propia documentación de Windows de la ASF dice que ellos no publican binarios y nombra Apache Lounge como opción habitual: <https://httpd.apache.org/docs/current/platform/windows.html>. Apache Lounge publica ZIP Win64 (en esa fecha, builds VS18 de 2.4.69) con checksum y PGP: <https://www.apachelounge.com/download/>. macOS y Linux: no hay binario oficial reubicable. |
| PHP | Fuente oficial en <https://www.php.net/downloads.php>. Binarios Windows oficiales del equipo de PHP en <https://windows.php.net/download/> y <https://windows.php.net/downloads/releases/>, con sumas publicadas (el índice histórico usa SHA-1; las páginas de release y empaquetadores citan SHA-256). PHP distingue NTS (CLI y FastCGI) y TS (módulo de Apache). | Windows: el ZIP es reubicable. Que traiga las DLL del runtime de Visual C++ junto al ejecutable queda por verificar. macOS y Linux: php.net no publica binarios; indica el fuente o el gestor de paquetes de la distro. Esos paquetes instalan en rutas de sistema. |
| MariaDB | <https://mariadb.org/download/> y el archivo <https://archive.mariadb.org/>. Documentación de paquetes: <https://mariadb.com/docs/server/server-management/install-and-upgrade-mariadb/installing-mariadb/binary-packages>. | Windows: ZIP oficial, instalación manual sin servicio de Windows. Linux: tarball genérico (`bintar`), instalable en el home sin root; hay variantes por glibc y por systemd. macOS: la documentación actual describe Homebrew y un PKG. El PKG escribe fuera de la carpeta de usuario. No hay un tarball genérico de macOS equivalente al de Linux. |

Los binarios de Windows de Apache Lounge y de PHP deben compartir familia de compilador. Un `mod_php` TS contra un httpd de otro Visual C++ es una fuente clásica de fallos. Por eso el [ADR 0005](0005-proyectos-y-plantillas.md) usa FastCGI y, en Windows, el ZIP **NTS** oficial, no el módulo TS.

## Decisión

Un módulo `runtimes` descarga, verifica y guarda. El supervisor y la UI dependen de su interfaz, no de URLs sueltas.

### Catálogo fijado

`catalog/runtimes.toml` en el repositorio es la lista blanca. Cada entrada tiene: componente, versión, sistema, arquitectura, URL HTTPS, nombre de archivo, algoritmo de hash, hash, licencia SPDX y notas (glibc mínima, Visual C++). El supervisor solo descarga entradas de ese archivo. Cambiar una URL o un hash es un commit revisado. No se scrapea la web del vendor en tiempo de ejecución para «la última versión».

El hash mínimo aceptado en el catálogo es SHA-256. Si el publicador solo ofrece SHA-1, una persona calcula el SHA-256 del archivo, lo comprueba contra el SHA-1 publicado y fija el SHA-256 en el catálogo.

La descarga llega a un temporal. Se calcula el hash. Si no coincide, se borra el temporal y se registra el error con la acción «volver a descargar o revisar el catálogo». Solo entonces se descomprime en un directorio nuevo y se renombra a la ruta final.

### Vigilancia del catálogo

Cuando el workflow semanal detecte una URL retirada o un hash que ya no coincide, la respuesta es actualizar `catalog/runtimes.toml` a la build vigente en un pull request revisado. No se guardan copias propias de binarios de terceros como respaldo: el instalador no redistribuye Apache, PHP ni MariaDB ([ADR 0009](0009-licencia.md)).

### Extracción en Unix (fase 1b)

En Linux y macOS la extracción del paquete deberá conservar los bits de ejecución de los binarios. Eso no se implementa en la fase 1 (Windows); queda como requisito de la 1b/1c.

### Dónde viven

Nunca en una ruta de sistema. Raíz de datos:

| Sistema | Carpeta |
|---|---|
| Windows | `%LOCALAPPDATA%\Hostig` |
| macOS | `~/Library/Application Support/Hostig` |
| Linux | `$XDG_DATA_HOME/hostig` o `~/.local/share/hostig` |

Runtimes: `{datos}/runtimes/{componente}/{versión}-{os}-{arch}/`. Varias versiones conviven como directorios hermanos. Borrar una versión es borrar ese directorio, y el supervisor lo rechaza si algún proyecto la tiene en ejecución.

El código del sitio lo elige el usuario, fuera de esta carpeta. Los datos de MariaDB, los logs y la configuración generada viven dentro.

### Fuentes que se usan

**Windows, ya decidido**

- PHP: ZIP NTS x64 oficial de windows.php.net. FastCGI con un pool de N procesos `php-cgi.exe` ([ADR 0005](0005-proyectos-y-plantillas.md)). static-php-cli no se usa en Windows: no genera `php-fpm` en ese sistema.
- MariaDB: ZIP winx64 oficial del archivo de mariadb.org. No el MSI, porque el MSI registra un servicio de Windows.
- Apache: ZIP Win64 de Apache Lounge, el builder que la documentación de Windows de la ASF nombra. El catálogo guarda su SHA-256. Se documenta como build de terceros a partir del fuente de la ASF, no como binario de la ASF.

### Runtime de Visual C++ en la carpeta del runtime

Microsoft permite el despliegue local de aplicación: copiar, sin modificarlas, las DLL que el redistribuible marca como redistribuibles, en la misma carpeta que el ejecutable. Así un usuario sin administrador, y una aplicación en un medio extraíble, pueden arrancar. El instalador central `vc_redist.x64.exe` escribe en `%windir%\system32` y pide administrador. Microsoft no recomienda la copia local porque Windows Update no la parchea: la aplicación tiene que publicar ella la actualización. No se redistribuyen DLL de depuración.

Fuentes:

- <https://learn.microsoft.com/en-us/cpp/windows/redistributing-visual-cpp-files>
- <https://learn.microsoft.com/en-us/cpp/windows/determining-which-dlls-to-redistribute>
- <https://learn.microsoft.com/en-us/cpp/windows/deployment-in-visual-cpp>
- <https://learn.microsoft.com/en-us/cpp/windows/choosing-a-deployment-method>
- <https://learn.microsoft.com/en-us/cpp/windows/walkthrough-deploying-a-visual-cpp-application-to-an-application-local-folder>

Decisión: las DLL de esa lista que el build de Apache Lounge necesite viajan dentro de `{datos}/runtimes/.../`, junto a `httpd.exe`. Si el ZIP de Apache Lounge ya las trae, se quedan. Si falta alguna de la lista del redistribuible de esa versión de Visual C++, el catálogo la añade desde ese redistribuible, sin ejecutarlo. Que el ZIP oficial de PHP traiga las suyas junto a `php.exe` queda por verificar. Subir el catálogo sustituye también esas DLL. No se lanza `vc_redist` y no se escribe en `system32`. Con eso el punto 5 del MVP (sin administrador) se cumple también para el runtime de C++.


**Linux, MariaDB, ya decidido**

- Tarball genérico oficial (`linux-systemd` cuando la glibc del sistema llega al mínimo anotado en el catálogo). Se invoca siempre con `--defaults-file` apuntando a nuestra configuración, para no leer `/etc/my.cnf` ni `~/.my.cnf` ajenos. La documentación del bintar advierte justo de esa mezcla.

**Builds de CI desde el fuente oficial, aceptados por fases**

No hay archivo oficial reubicable de Apache en macOS ni en Linux, ni de PHP en esos dos sistemas, ni de MariaDB en macOS. Queda aceptado construirlos en CI, por fases, a partir del tarball oficial:

1. El CI descarga el tarball de fuente oficial (ASF para httpd, php.net para PHP, mariadb.org para MariaDB en macOS).
2. Verifica el SHA-256 publicado y, donde exista, la firma PGP contra el fichero `KEYS` del proyecto.
3. Compila un prefijo reubicable (`mod_proxy_fcgi` en Apache; `php-fpm` en Unix, con la salvedad de PHP que sigue).
4. Publica ese prefijo en una release de GitHub distinta del instalador de la app, con su SHA-256 en el catálogo y las licencias de terceros dentro del prefijo.

Eso no mete binarios de terceros en el instalador. Sí publica binarios que nosotros compilamos. El [ADR 0009](0009-licencia.md) exige que el fuente exacto sea el tarball oficial ya verificado y que, para MariaDB, el fuente exacto viaje al lado (GPLv2).

La fase 1 (Windows x64) no necesita esos prefijos: usa los ZIP ya decididos. La fase 1b (Linux) publica Apache y PHP. MariaDB en Linux sigue siendo el bintar oficial. La fase 1c (macOS) publica Apache, PHP y MariaDB. El CI de la aplicación, en cambio, compila y pasa tests en los tres sistemas desde el principio ([ADR 0010](0010-distribucion.md)).

### static-php-cli, antes de dar por buena la compilación manual de PHP

Antes de tratar una compilación manual de PHP como la vía de Linux y macOS, se compara con [static-php-cli](https://github.com/crazywhalecc/static-php-cli) (documentación en <https://static-php.dev>).

| | static-php-cli | Compilación manual del tarball |
|---|---|---|
| Licencia de la herramienta | MIT, copyright Jerry Ma 2022-2023. PHP y las extensiones conservan su licencia. `spc dump-license` las exporta. | No añade una herramienta. Hay que archivar igual la licencia PHP 3.01 y la de cada biblioteca embebida. |
| Verificabilidad | El modelo de artefactos de la rama v3 descarga el fuente oficial de php.net y comprueba el SHA-256 (<https://static-php.dev/en/develop/artifact-model.html>). El issue [#958](https://github.com/crazywhalecc/static-php-cli/issues/958) (2025-11-22) dice que, en ese momento, solo el fuente de PHP y los `ghrel` tenían esa verificación unificada: el resto de dependencias, no. Pasar la versión `git` clona master y no sirve. `--prefer-binary` y una URL propia debilitan la comprobación. | El CI verifica él mismo el tarball de php.net y cada dependencia que enlace. Más trabajo, y el control está en nuestro script. |
| Extensiones | Más de cien, entre ellas mysqli, mysqlnd, pdo_mysql, openssl, curl, gd, mbstring, intl, opcache, zip y zlib (<https://static-php.dev/en/guide/extensions.html>). El destino Linux por defecto, musl estático, desactiva `dl()` y no carga un `.so` externo. El destino glibc (`SPC_TARGET=native-native-gnu.2.17`) sí puede. En Windows el binario estático no carga las DLL oficiales de PHP: las extensiones tienen que ir compiladas dentro. | Las mismas extensiones, con los flags que escribamos. También hay que fijar musl o glibc a conciencia. |
| php-fpm | Sí en Linux y macOS, con `spc build:php "..." --build-fpm`. El binario queda en `buildroot/bin/php-fpm`. No en Windows. | Sí donde el fuente de PHP lo permite, que en Windows no es el caso del ZIP oficial: allí el producto sigue en `php-cgi`. |

Decisión: en Linux (fase 1b) y macOS (fase 1c) el constructor preferido de PHP es una versión fijada de static-php-cli, no el binario nocturno de `dl.static-php.dev`. El CI fija el SHA-256 del tarball oficial de php.net y falla si no coincide, guarda las sumas, y el catálogo fija el hash del `php-fpm` resultante. No se usa la versión `git` ni `--prefer-binary`. Se archiva la salida de `spc dump-license`. La compilación manual del tarball queda de reserva solo si esa versión fijada de static-php-cli no puede emitir `php-fpm` para la versión de PHP fijada. Apache en Linux y macOS, y MariaDB en macOS, siguen compilándose en CI desde el fuente oficial verificado: static-php-cli no los construye. Windows no cambia: ZIP NTS oficial y pool de `php-cgi`.

### Eficiencia

Solo se descarga la versión que un arranque o un proyecto pide. El disco puede guardar varias. En ejecución, el [ADR 0005](0005-proyectos-y-plantillas.md) no levanta un servidor por cada versión descargada.

## Consecuencias

- El primer arranque necesita red y espacio (cada stack son cientos de MB).
- Un cambio de compilador en Apache Lounge (VS17 a VS18, por ejemplo) es un cambio de catálogo, no un detalle invisible.
- La glibc del bintar de MariaDB puede dejar fuera distros muy viejas. El catálogo lo declara y el error dice qué versión de glibc hace falta.
- Construir Apache y PHP en CI alarga el pipeline y nos hace responsables del prefijo (OpenSSL incluido, avisos de seguridad cuando el fuente upstream publique un parche).

## Alternativas descartadas

- `apt`, Homebrew o el MSI de MariaDB como instalación por defecto. Escriben fuera de la carpeta del usuario y chocan con la regla de no tocar el sistema.
- Meter los binarios en el repositorio o en el instalador de la app. Rompe la política de no redistribuir dentro del producto y hincha cada release.
- `mod_php` con el build TS de Windows. Ata PHP a un solo httpd y a un solo Visual C++, y choca con «cada proyecto, su PHP».
- Ejecutar `vc_redist.x64.exe`. Pide administrador y escribe fuera de la carpeta de datos. La licencia permite llevar las DLL en la carpeta del runtime, que es lo que hace este ADR.
- Confiar en el binario nocturno de static-php-cli, o en `--prefer-binary`, como prueba de que el fuente es el oficial.

