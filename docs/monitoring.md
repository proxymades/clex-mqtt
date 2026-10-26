# Мониторинг CLEX MQTT

## Доступ

Метрики доступны на HTTP-интерфейсе брокера по `GET /metrics`. Задайте `CLEX_METRICS_TOKEN` в закрытом окружении и передавайте его заголовком `Authorization: Bearer <token>`. Для локальной конфигурации команда `python3 scripts/metrics_token.py` добавляет новый случайный токен в `.env` с правами `0600`, сохраняя существующее значение. При запуске брокер хранит SHA-256 токена; сравнение выполняется за постоянное время. API панели выдаёт только состояние включения и путь экспорта.

В Prometheus положите это значение в отдельный файл `/etc/prometheus/secrets/clex-mqtt-token`, доступный сервисному пользователю мониторинга. Пример `deploy/prometheus/prometheus.yml` использует `authorization.credentials_file`, период 15 секунд и timeout 5 секунд. Настройка заголовка описана в [документации Prometheus](https://prometheus.io/docs/prometheus/latest/configuration/configuration/#http_config).

Шаблон Nginx закрывает `/metrics` на публичном домене. Для сбора используйте loopback HTTP-интерфейс или отдельный защищённый маршрут мониторинга. При размещении Prometheus на другом узле используйте приватную сеть и HTTPS. В контейнере адрес цели указывает на доступный из контейнера интерфейс брокера.

Ответ имеет Content-Type `application/openmetrics-text; version=1.0.0; charset=utf-8` и `Cache-Control: no-store`. Формат создаёт библиотека `prometheus-client`. При отсутствии настройки endpoint возвращает 404, при ошибке токена – 401. Авторизация выполняется до обращения к БД. Административная cookie и токен мониторинга имеют отдельные назначения.

## Метрики

Все имена имеют префикс `clex_mqtt_`. Имена пользователей, Client ID, топики, адреса и payload доступны в защищённых представлениях панели; метрики агрегируются по фиксированным измерениям. В `delivery_queue_messages` используются четыре фиксированные фазы. `build_info` содержит версию продукта.

| Имя после префикса | Значение |
|---|---|
| `connections` | Активные аутентифицированные MQTT-подключения |
| `connection_attempts_total` | Сокеты, допущенные к MQTT-handshake |
| `connections_authenticated_total` | Успешные handshake, включая takeover |
| `handshake_failures_total` | Ошибки и таймауты MQTT-handshake |
| `connection_capacity_rejections_total` | Отказы по лимиту соединений MQTT |
| `packets_received_total`, `packets_sent_total` | MQTT-пакеты, включая управляющие |
| `publish_packets_accepted_total` | Принятые PUBLISH, включая повторы QoS |
| `publish_packets_rejected_total` | Этапы публикации, отклонённые ACL или квотой |
| `publications_committed_total` | Зафиксированные публикации, включая импорт мостов и LWT |
| `wire_bytes_received_total`, `wire_bytes_sent_total` | Учтённые байты MQTT wire-обмена |
| `storage_lock_wait_seconds` | Гистограмма ожидания начала транзакции, включая пул и advisory lock |
| `storage_commit_seconds` | Гистограмма завершённых попыток COMMIT состояния |
| `storage_commits_total`, `storage_commit_errors_total`, `storage_commit_unknown_total` | Успех, ошибки и неизвестный результат COMMIT |
| `stored_sessions`, `subscriptions`, `retained_messages` | Состояние долговечного хранилища |
| `delivery_queue_messages{state=…}`, `delivery_queue_bytes`, `incoming_qos2` | Очереди MQTT по фазам и ожидающие PUBREL |
| `bridges`, `bridges_enabled`, `bridges_connected` | Состояния настроенных мостов |
| `bridge_queue_messages`, `bridge_queue_bytes` | Исходящие обязательства мостов |
| `database_up`, `database_snapshot_timestamp_seconds` | Результат последнего сбора и время последнего успешного снимка |
| `database_pool_connections`, `database_pool_idle_connections` | Размер и свободная часть пула PostgreSQL |
| `monitor_events`, `monitor_accounted_bytes` | Размер ограниченного буфера предпросмотра |

Counter отражают один запуск процесса; рестарт начинает новый ряд значений. PUBLISH QoS 2 принимается на шаге PUBREC, а событие зафиксированной публикации появляется на первом успешном PUBREL. Счётчики сообщений и пакетов имеют разные единицы. Время COMMIT относится к верхнеуровневым транзакциям слоя состояния; административные операции и savepoint учитываются своими вызывающими операциями. Сбор ожидания записывает успешно открытые транзакции.

Состояние БД собирается одной read-only SQL-командой с согласованным MVCC-снимком. Timeout команды – 1500 мс, всего сбора – 2 секунды; снимок кешируется на 5 секунд. При ошибке экспортер отдаёт оперативные метрики процесса, `database_up=0` и прежний успешный снимок с его исходным временем. До первого успешного снимка долговечные gauges появляются после доступного сбора. Параллельный первый запрос получает 503 с `Retry-After: 1`; последующие запросы используют кеш. Это ограничивает влияние сбора на MQTT.

## Запросы и оповещения

Скорость зафиксированных публикаций:

```promql
rate(clex_mqtt_publications_committed_total[5m])
```

p95 ожидания транзакции:

```promql
histogram_quantile(0.95, sum by (le, job, instance) (rate(clex_mqtt_storage_lock_wait_seconds_bucket[5m])))
```

Возраст снимка состояния БД:

```promql
time() - clex_mqtt_database_snapshot_timestamp_seconds
```

В `deploy/prometheus/clex-mqtt-alerts.yml` находятся начальные правила доступности endpoint, сбора БД, очереди, мостов и ожидания блокировки. Подберите пороги под число клиентов и профиль нагрузки. Перед подключением конфигурации выполните `promtool check config --syntax-only deploy/prometheus/prometheus.yml` и `promtool check rules deploy/prometheus/clex-mqtt-alerts.yml`.

## Локальный стек Ubuntu 24.04

Пакеты `prometheus`, `prometheus-node-exporter` и `prometheus-alertmanager` предоставляют сбор метрик, показатели ОС и доставку сообщений. Перед настройкой новых служб можно временно маскировать их через systemd; после проверки конфигураций снимите маски. Используйте штатные unit пакетов, задавая параметры через `/etc/default/prometheus`, `/etc/default/prometheus-node-exporter` и `/etc/default/prometheus-alertmanager`.

- Prometheus: `--config.file=/etc/prometheus/prometheus.yml --storage.tsdb.path=/var/lib/prometheus/clex-mqtt --storage.tsdb.retention.time=7d --web.listen-address=127.0.0.1:9090`.
- Node Exporter: `--web.listen-address=127.0.0.1:9100`.
- Alertmanager: `--config.file=/etc/prometheus/alertmanager.yml --storage.path=/var/lib/prometheus/alertmanager --web.listen-address=127.0.0.1:9093 --cluster.listen-address=`.

Создайте каталоги данных с владельцем штатного пользователя `prometheus`. Каталог секретов `/etc/prometheus/secrets` имеет права `root:prometheus 0750`, файлы токенов – `root:prometheus 0640`. Переносите значение `CLEX_METRICS_TOKEN` из закрытого EnvironmentFile без вывода в консоль; URL и аргументы процессов не должны содержать токены. `/metrics` брокера остаётся закрытым на публичном Nginx.

Шаблон сбора содержит три задания: сам Prometheus, Node Exporter и CLEX MQTT. После запуска проверьте `/api/v1/targets`: все три источника должны иметь состояние `up`. Для просмотра интерфейса используйте SSH-туннель к loopback-порту сервера; дополнительные публичные порты открывать не требуется.

## Telegram

`alertmanager.example.yml` – пример с демонстрационным Chat ID. Реальный ID и токен хранятся только на сервере, вне репозитория и build context. Токен полностью включает цифровой префикс и двоеточие. Для приёма личных сообщений сначала откройте чат с ботом. Проверяйте принадлежность токена через `getMe`, а получателя привязывайте по уникальному сообщению-подтверждению. Не изменяйте существующий webhook или чужой процесс опроса бота без отдельной настройки его владельцем.

Alertmanager 0.26 поддерживает `bot_token_file`. Шаблон использует простой текст (`parse_mode` пустой), отправляет уведомления о срабатывании и восстановлении, объединяет события по имени и источнику. Первое сообщение отправляется после 10 секунд ожидания группы, изменения группы обрабатываются раз в минуту, повтор неизменённой проблемы – раз в четыре часа.

После установки своего получателя включите секцию `alerting` в Prometheus с адресом `127.0.0.1:9093`, проверьте конфигурации через `promtool check config` и `amtool check-config` (в некоторых пакетах утилита называется `prometheus-amtool`), затем выполните reload служб. Проверяйте всю цепочку временным явно помеченным правилом: отправка в чат должна пройти именно от Prometheus через Alertmanager. После проверки удалите правило и его ссылку из конфигурации.

`host-alerts.yml` добавляет предупреждения о недоступном Node Exporter, свободном месте, памяти, длительной нагрузке CPU и запасе inode. Начальные пороги требуют адаптации к эксплуатации. На одном сервере остановка всего VPS одновременно останавливает локальные средства мониторинга и доставки; контроль такой недоступности выполняется из независимого окружения.

## Контроль резервных копий

После установки локальных копий включите Textfile Collector Node Exporter параметром `--collector.textfile.directory=/var/lib/prometheus/node-exporter/clex-mqtt`. Создайте этот отдельный каталог с правами `root:root 0755`; существующие закрытые каталоги копий сохраняют права `0700`. При уже используемом каталоге Textfile Collector согласуйте путь вывода и unit с существующей настройкой.

Из корня исходников выполните `bash deploy/backup/install-monitoring.sh`. Скрипт сначала проверяет правила и их сценарии через установленный `promtool`, затем устанавливает отдельный сборщик и минутный таймер, добавляет файл правил в Prometheus и выполняет reload. При ошибке проверки или reload прежняя конфигурация Prometheus восстанавливается. Создание копий, их ротация и процесс брокера при этой установке не изменяются.

Сборщик читает только метаданные завершённых комплектов и свойства `clex-mqtt-backup.service`. Он не подключается к PostgreSQL и не читает содержимое дампа или пароли. Файл `clex-mqtt-backup.prom` заменяется атомарно и содержит только числовые показатели с фиксированными именами:

| Метрика | Значение |
|---|---|
| `clex_mqtt_backup_last_success_timestamp_seconds` | Время последней опубликованной копии из manifest |
| `clex_mqtt_backup_completed_copies` | Число завершённых комплектов |
| `clex_mqtt_backup_last_run_success` | Результат последней завершённой попытки; во время повторного запуска сохраняется предыдущий результат |
| `clex_mqtt_backup_status_timestamp_seconds` | Время обновления файла состояния |
| `clex_mqtt_backup_status_collection_success` | Успех чтения метаданных и результата службы |

`backup-alerts.yml` содержит три правила: ошибка копирования в течение двух минут, отсутствие успешной копии более 26 часов с выдержкой пять минут и недоступное либо устаревшее состояние сборщика с выдержкой пять минут. Последнее правило срабатывает при работающем Node Exporter, если сбор ошибочен, метрики отсутствуют или файл не обновлялся более трёх минут. Порог 26 часов рассчитан на ежедневное расписание и требует изменения при другом периоде копирования.

Минутный сбор не выполняет повторную проверку контрольных сумм архивов. Для проверки содержимого используйте `clex-mqtt-backup verify` и пробное восстановление. Доставка этих предупреждений использует уже настроенный маршрут Alertmanager.

`python3 tests/test_backup_status.py` проверяет метаданные, ошибки, повторные попытки и атомарную запись. При доступном `promtool` дополнительно выполняются сценарии правил из `backup-alerts.test.yml`; установщик всегда выполняет их на целевой системе до изменения служб.

## Grafana

Grafana предоставляет отдельный интерфейс графиков. Она получает данные от локального Prometheus; история метрик остаётся в Prometheus. SQLite-база Grafana хранит её аккаунты, настройки и панели.

Шаблоны в `deploy/grafana` проверены с Grafana OSS 13.2.4 и Prometheus 2.45.3. Они используют встроенные панели и источник Prometheus, без установки дополнительных плагинов.

### Установка и доступ

Установите Grafana OSS из [официального APT-репозитория](https://grafana.com/docs/grafana/latest/setup-grafana/installation/debian/). До первого запуска новой установки заблокируйте службу командой `systemctl mask --runtime grafana-server.service`, чтобы сначала подготовить отдельные учётные данные и локальный слушатель.

Если на узле отсутствует рабочий IPv6-доступ, для отдельной команды APT можно использовать `-o Acquire::ForceIPv4=true`. Изменять сетевые настройки узла для этого не требуется.

После установки пакета создайте каталоги:

```bash
install -d -o root -g grafana -m 0750 \
    /etc/grafana/secrets /etc/grafana/dashboards \
    /etc/grafana/provisioning/datasources /etc/grafana/provisioning/dashboards
```

Для новой установки создайте начальный пароль и ключ шифрования. Существующие файлы команда не перезаписывает:

```bash
python3 - <<'PY'
import grp, os, secrets
from pathlib import Path
os.umask(0o077)
group = grp.getgrnam('grafana').gr_gid
for name, value in [('admin-password', secrets.token_urlsafe(30)),
                    ('secret-key', secrets.token_hex(32))]:
    path = Path('/etc/grafana/secrets') / name
    with path.open('x') as target:
        target.write(value + '\n')
    os.chown(path, 0, group)
    path.chmod(0o640)
print('Grafana secrets: OK')
PY
```

Начальный логин – `admin`, пароль находится в закрытом файле `/etc/grafana/secrets/admin-password`. Это отдельная учётная запись Grafana; пароли CLEX MQTT и MQTT-устройств для неё не используются. Начальный пароль применяется при создании базы Grafana; изменение этого файла не сбрасывает пароль существующего пользователя.

Скопируйте `deploy/grafana/grafana.ini.example` в `/etc/grafana/grafana.ini` с правами `root:grafana 0640`. Замените `mqtt.example.com` на свой домен в `domain` и `root_url`. Шаблон:

- слушает только `127.0.0.1:3000`;
- использует HTTPS-адрес `https://mqtt.example.com/grafana/` и отдельную cookie;
- отключает самостоятельную регистрацию и анонимный просмотр;
- сохраняет защиту от перебора пароля;
- сохраняет действующую цепочку Prometheus → Alertmanager; отдельный движок оповещений Grafana отключён.

Скопируйте файлы предоставления данных и панелей:

```bash
install -o root -g grafana -m 0640 deploy/grafana/datasource.yml \
    /etc/grafana/provisioning/datasources/clex-mqtt.yml
install -o root -g grafana -m 0640 deploy/grafana/dashboards.yml \
    /etc/grafana/provisioning/dashboards/clex-mqtt.yml
install -o root -g grafana -m 0640 deploy/grafana/dashboards/*.json \
    /etc/grafana/dashboards/
install -d -m 0755 /etc/systemd/system/grafana-server.service.d
install -m 0644 deploy/grafana/resources.conf \
    /etc/systemd/system/grafana-server.service.d/20-resources.conf
```

Пределы памяти в `resources.conf` рассчитаны на небольшой сервер; при другом составе служб адаптируйте их к доступной памяти.

Добавьте locations из `deploy/grafana/nginx-location.conf` в существующий HTTPS-блок Nginx. Прокси сохраняет префикс `/grafana/`, который обрабатывается настройкой `serve_from_sub_path`. Для WebSocket Grafana Live предусмотрен отдельный маршрут. Открывать порт 3000 в межсетевом экране не требуется.

```bash
systemctl unmask --runtime grafana-server.service
systemctl daemon-reload
systemctl enable --now grafana-server.service
nginx -t && systemctl reload nginx
curl --fail https://mqtt.example.com/grafana/api/health
```

Проверьте вход и все три панели. Без авторизации содержимое панелей и API данных должны быть недоступны.

### Панели

В папке **CLEX MQTT** доступны:

- **Брокер** – доступность, подключения, сессии, публикации, трафик, очереди, задержки хранилища и мосты.
- **Сервер** – CPU, память, диск, swap, сеть и средняя нагрузка.
- **Резервные копии** – последняя попытка, возраст и количество копий, состояние сборщика и активные предупреждения.

Панели обновляются раз в 30 секунд, начальный диапазон – последние 6 часов. Срок истории определяется настройкой хранения Prometheus. Если в окне не было операций, график p95 может не иметь значений.

Файлы панелей предоставляются из конфигурации и обновляются вместе с ней. Для собственных вариантов создайте отдельные копии панелей. Подробнее – [предоставление конфигурации Grafana](https://grafana.com/docs/grafana/latest/administration/provisioning/).

### Контроль доступности Grafana

Добавьте содержимое `deploy/grafana/prometheus-job.yml` в существующий список `scrape_configs` Prometheus. Скопируйте `deploy/grafana/grafana-alerts.yml` в `/etc/prometheus/grafana-alerts.yml` и добавьте этот путь в `rule_files`. Существующие цели и правила сохраняются.

```bash
promtool check rules /etc/prometheus/grafana-alerts.yml
promtool check config /etc/prometheus/prometheus.yml
systemctl reload prometheus
```

Цель `grafana` должна иметь `up = 1`. Правило `GrafanaDown` предупреждает о недоступности более двух минут через действующий маршрут Alertmanager. Публичный путь `/grafana/metrics` закрыт шаблоном Nginx; Prometheus читает метрики напрямую по loopback.

### Резервная копия Grafana

До запуска основной копии `grafana-snapshot.py` получает согласованный SQLite-снимок через штатный backup API и проверяет его целостность. Копировать активный файл `grafana.db` обычной файловой командой для этой процедуры не требуется.

При уже установленной системе локальных копий:

```bash
install -m 0755 deploy/grafana/grafana-snapshot.py \
    /usr/local/sbin/clex-mqtt-grafana-snapshot
install -d -m 0700 /var/lib/clex-mqtt-grafana-backup
/usr/local/sbin/clex-mqtt-grafana-snapshot
install -d -m 0755 /etc/systemd/system/clex-mqtt-backup.service.d
install -m 0644 deploy/grafana/backup-service.conf \
    /etc/systemd/system/clex-mqtt-backup.service.d/30-grafana.conf
```

Сохраните остальные drop-in файлы этой службы. Добавьте в `sources` закрытого `/etc/clex-mqtt/backup.json` следующие обязательные источники, сохранив прежние источники и ротацию:

```json
[
  {"path": "/etc/grafana", "required": true},
  {"path": "/etc/prometheus", "required": true},
  {"path": "/var/lib/clex-mqtt-grafana-backup/grafana.db", "required": true},
  {"path": "/usr/local/sbin/clex-mqtt-grafana-snapshot", "required": true},
  {"path": "/etc/systemd/system/clex-mqtt-backup.service.d", "required": true},
  {"path": "/etc/systemd/system/grafana-server.service.d", "required": true}
]
```

После изменения выполните `systemctl daemon-reload`, запустите `clex-mqtt-backup.service` и проверьте новый комплект командой `clex-mqtt-backup verify`. Пароль, ключ шифрования и SQLite-снимок остаются внутри закрытого архива; их нельзя публиковать.

Для восстановления остановите Grafana и сохраните прежний каталог её данных. Восстановите конфигурацию и снимок из одного проверенного комплекта в чистый каталог данных; снимок должен находиться по `/var/lib/grafana/grafana.db` с владельцем `grafana:grafana` и правами `0640`. Конфигурация и файлы секретов должны быть доступны группе `grafana` без доступа остальных пользователей. После запуска проверьте вход, источник данных и панели.
