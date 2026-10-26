# Локальные резервные копии CLEX MQTT

Механизм для отдельного Ubuntu VPS с PostgreSQL 17 создаёт копии без остановки брокера. Сохраняются семь последних успешно созданных и проверенных комплектов. Фиксированного ограничения в гигабайтах нет. Для замены требуется временное место под новую, восьмую копию: предыдущая удаляется после её проверки и публикации. При нехватке места незавершённый комплект удаляется, прежние сохраняются.

Локальная копия помогает восстановиться после ошибочного изменения или удаления данных. Потеря самого диска или VPS затрагивает и копии на нём. Перенос завершённых комплектов на другой носитель можно добавить отдельно.

## Состав комплекта

- `database.dump` – согласованный снимок одной БД в сжатом PostgreSQL custom format.
- `database-role.json` – параметры и парольный verifier только выделенной роли брокера.
- `files.tar.gz` – конфигурация, текущий Linux-бинарник, unit systemd, конфигурация Nginx и выбранные сертификатные файлы.
- `manifest.json` – время UTC, версия PostgreSQL, размеры и SHA-256 файлов.

Каталоги имеют права `0700`, файлы комплектов – `0600`. Внутри находятся секреты, включая ключ мостов и ключи TLS. Комплекты исключаются из Git и Docker build context. Скрипт не выводит пароли, токены, URL БД и содержимое роли.

Список файлов задаётся в `deploy/backup/backup.example.json`. Шаблон рассчитан на отдельный VPS и включает весь `/etc/letsencrypt`; на совместно используемом сервере ограничьте список сертификатами и конфигурацией этого сервиса. Обязательные источники должны существовать; отсутствие необязательных допускается. Изменение исходных файлов во время копирования прерывает публикацию комплекта.

## Установка на Ubuntu

Команды выполняются под `root` из корня исходников после настройки PostgreSQL, брокера и Nginx. Перед установкой проверьте пути, порт и имя кластера. Скрипт использует локальный Unix socket и peer-аутентификацию ОС-пользователя `postgres`, без пароля в аргументах процесса. Для смены ОС-пользователя unit явно задаёт `AmbientCapabilities=CAP_SETUID CAP_SETGID`, сохраняя остальные ограничения службы. Системные ошибки показывают класс и код errno без содержимого конфигурации и секретных значений.

```bash
install -d -o root -g root -m 0700 /var/backups/clex-mqtt
install -m 0755 deploy/backup/clex-mqtt-backup.py /usr/local/sbin/clex-mqtt-backup
if [ ! -e /etc/clex-mqtt/backup.json ]; then
    install -m 0600 deploy/backup/backup.example.json /etc/clex-mqtt/backup.json
fi
install -m 0644 deploy/systemd/clex-mqtt-backup.service /etc/systemd/system/
install -m 0644 deploy/systemd/clex-mqtt-backup.timer /etc/systemd/system/
install -m 0644 docs/backups.md /opt/clex-mqtt/docs/backups.md
systemd-analyze verify /etc/systemd/system/clex-mqtt-backup.service /etc/systemd/system/clex-mqtt-backup.timer
systemctl daemon-reload
systemctl start clex-mqtt-backup.service
```

Для стандартной установки с кластером `17/main` все действия, первая копия и включение таймера объединены в `bash deploy/backup/install.sh`. Существующая конфигурация `backup.json` сохраняется. При другой структуре используйте отдельные команды выше и адаптируйте unit.

После успешной первой копии включите расписание:

```bash
systemctl enable --now clex-mqtt-backup.timer
systemctl list-timers clex-mqtt-backup.timer --no-pager
journalctl -u clex-mqtt-backup.service -n 30 --no-pager
/usr/local/sbin/clex-mqtt-backup list
```

Таймер запускается ежедневно в 22:30 UTC с задержкой до 15 минут. Пропущенный из-за выключения сервера запуск выполняется после включения. После ошибки служба повторяет попытку через 15 минут; ограничение частоты запусков отключено. Один запуск ограничен шестью часами, что можно изменить в unit для другого профиля БД. Одновременные копирования блокируются файловой блокировкой. Скрипт не запускает и не перезапускает брокер.

## Проверка и ротация

Перед публикацией проверяются контрольные суммы всех файлов, полный gzip-поток файлового архива, безопасные имена/ссылки в tar и чтение всего дампа через `pg_restore --file=/dev/null`. Последняя команда не подключается к БД и не выполняет SQL. Это проверка читаемости; отдельное пробное восстановление проверяет применение дампа на PostgreSQL.

Рабочий каталог `.pending-*` после проверки атомарно переименовывается в `backup-<UTC>-<id>`. Затем удаляются самые старые комплекты сверх `keep=7`. Ошибка до публикации сохраняет все завершённые копии. После принудительного завершения процесса временный каталог очищается при следующем запуске. Неизвестные каталоги и прежние ручные копии вне этого каталога не удаляются. Ошибка удаления старого комплекта может временно оставить больше семи копий; повторный запуск снова применит ротацию.

Для повторной проверки укажите существующий каталог из команды `list`:

```bash
/usr/local/sbin/clex-mqtt-backup verify /var/backups/clex-mqtt/backup-YYYYMMDDTHHMMSS.ffffffZ-xxxxxxxx
```

Размеры можно посмотреть без чтения содержимого:

```bash
du -sh /var/backups/clex-mqtt
df -h /var/backups/clex-mqtt
```

## Уведомления

Состояние последней попытки и возраст последней успешной копии можно передавать через Node Exporter в Prometheus и Alertmanager. Отдельный минутный сборщик и установка описаны в [руководстве мониторинга](monitoring.md#контроль-резервных-копий). Механизм создания и ротации копий продолжает работать независимо от мониторинга.

## Пробное восстановление

Используйте отдельный PostgreSQL и подготовленный сервер для восстановления, не рабочую БД. Перенесите полный комплект по защищённому каналу, сохранив доступ только администратору. Установите PostgreSQL 17 и скрипт проверки, выполните `verify`. Задайте путь к перенесённому комплекту:

```bash
clex_backup_dir=/var/backups/clex-mqtt/backup-YYYYMMDDTHHMMSS.ffffffZ-xxxxxxxx
```

На новом экземпляре создайте выделенную роль с прежним verifier и пустую базу. Следующий пример рассчитан на штатную непривилегированную роль CLEX MQTT; существующие роль или база вызовут ошибку и не перезаписываются. Verifier передаётся psql через stdin и не печатается:

```bash
python3 - "$clex_backup_dir" <<'PY'
import json
from pathlib import Path
import re
import subprocess
import sys

directory = Path(sys.argv[1])
role = json.loads((directory / 'database-role.json').read_text())
manifest = json.loads((directory / 'manifest.json').read_text())
name, database = role['name'], manifest['database']
if not all(re.fullmatch(r'[A-Za-z_][A-Za-z0-9_]{0,62}', value) for value in (name, database)):
    raise SystemExit('Invalid database or role name.')
if any(role.get(flag) for flag in ('superuser', 'createdb', 'createrole', 'replication', 'bypassrls')):
    raise SystemExit('Review the privileged source role before restoration.')
password = role.get('password')
if not isinstance(password, str) or not password.startswith('SCRAM-SHA-256$'):
    raise SystemExit('Expected a SCRAM password verifier.')
password = password.replace("'", "''")
sql = (f'CREATE ROLE "{name}" LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE '
       f"NOREPLICATION NOBYPASSRLS PASSWORD '{password}';\n"
       f'CREATE DATABASE "{database}" OWNER "{name}" TEMPLATE template0 ENCODING \'UTF8\';\n'
       f'REVOKE ALL ON DATABASE "{database}" FROM PUBLIC;\n')
subprocess.run(['runuser', '-u', 'postgres', '--', 'psql', '-X', '-v', 'ON_ERROR_STOP=1', '-d', 'postgres'],
               input=sql, text=True, check=True)
PY
```

Для стандартных имён из шаблона восстановите таблицы и данные:

```bash
runuser -u postgres -- pg_restore --no-password --exit-on-error \
    --no-owner --role=clex_mqtt --dbname=clex_mqtt \
    < "$clex_backup_dir/database.dump"
```

Распакуйте файловую часть в отдельный закрытый каталог:

```bash
install -d -m 0700 /root/clex-mqtt-restore
tar -xzf "$clex_backup_dir/files.tar.gz" -C /root/clex-mqtt-restore --no-same-owner
```

Внутри сохранены пути относительно `/`: `etc/clex-mqtt`, `opt/clex-mqtt/clex-mqtt`, выбранные systemd/Nginx/Certbot-файлы. На целевом сервере создайте системного пользователя по [production-инструкции](deployment.md#staging-и-production-на-linux), установите бинарник и конфигурацию из распакованного каталога. Восстановите права: environment-файл `root:root 0600`, каталоги `/etc/clex-mqtt` и `certs` – `root:clex-mqtt 0750`, сертификат и ключ брокера – `root:clex-mqtt 0640`, бинарник – `root:root 0755`. Проверьте адрес БД в закрытом окружении для нового сервера, пути сертификатов и сетевой экран.

После проверки Nginx и unit запустите сервис. Проверьте readiness, административный вход, MQTT TLS и восстановление клиентских сессий и очередей. Зафиксируйте результат пробного восстановления отдельно от автоматической проверки архива. Восстановление возвращает состояние на момент снимка; сообщения и изменения после него в комплект не входят. Копия работающей БД согласована транзакционно, а файлы конфигурации копируются отдельно сразу после дампа.

### Проверка в отдельной БД на том же сервере

Для проверки применения дампа можно создать отдельную пустую БД с уникальным именем и отдельной непривилегированной ролью в существующем кластере PostgreSQL. Закройте подключение к ней для `PUBLIC`. Восстанавливайте через `pg_restore --single-transaction --no-owner --no-acl --role=TEST_ROLE --dbname=TEST_DATABASE`, явно указывая тестовую БД. Исходная рабочая БД в этой процедуре не изменяется.

Для проверки приложения используйте бинарник из того же комплекта и отдельный закрытый EnvironmentFile. Задайте новое подключение к тестовой БД, `CLEX_ENV=test`, свободные loopback-адреса MQTT и HTTP, соответствующий `CLEX_PUBLIC_URL` и `CLEX_COOKIE_SECURE=false`. Перенесите сохранённый `CLEX_BRIDGE_SECRET_KEY` из архивной конфигурации без вывода значения. До запуска отключите мосты только в тестовой БД, чтобы исключить передачу восстановленных сообщений внешним брокерам.

Размещайте тестовый бинарник в отдельном каталоге, например `/opt/clex-mqtt-restore-test`, с доступом сервисного пользователя. Проверяйте параметры монтирования: `/run` может иметь флаг `noexec` и подходит для закрытого временного EnvironmentFile, но в этом случае не позволяет исполнять бинарник. Для временного запуска через `systemd-run` задайте тип `exec`, чтобы ошибка исполнения не считалась успешным стартом.

Проверьте readiness, вход с восстановленной учётной записью, MQTT-пользователей, ACL и Retain. Для просмотра панели используйте SSH-туннель; локальный адрес браузера должен совпадать с тестовым `CLEX_PUBLIC_URL`. Пустые сессии или очереди в исходной копии не подтверждают восстановление этих состояний. Проверка на существующем кластере также не заменяет восстановление всего окружения на новом сервере.

После проверки остановите временный сервис, удалите только созданные тестовую БД и роль, затем тестовый бинарник, EnvironmentFile и служебный файл с именем тестовой БД. Перед удалением проверяйте уникальное имя и префикс тестовых объектов. Закройте SSH-туннель и проверьте готовность рабочего брокера.

## Проверки разработчика

`python3 tests/test_backups.py` проверяет ротацию 7/8/9, ошибки дампа/архива/диска, контрольные суммы, параллельный запуск и сохранение старых копий. Реальная PostgreSQL-проверка создаёт собственную случайную схему только в выделенной `TEST_DATABASE_URL`, снимает дамп, восстанавливает схему и сверяет бинарные данные. Проверка включена в `python3 scripts/check.py`.

## Документация поставщиков

- [PostgreSQL 17: pg_dump](https://www.postgresql.org/docs/17/app-pgdump.html)
- [PostgreSQL 17: pg_restore](https://www.postgresql.org/docs/17/app-pgrestore.html)
- [systemd: таймеры](https://www.freedesktop.org/software/systemd/man/systemd.timer.html)
