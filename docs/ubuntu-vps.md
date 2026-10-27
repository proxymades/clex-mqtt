# Подготовка VPS на Ubuntu 24.04

Пошаговая подготовка отдельного VPS для CLEX MQTT: DNS, SSH, сетевой экран, HTTPS, PostgreSQL, Linux-сборка и запуск службы. Общие требования и шаблоны production-конфигурации описаны в [руководстве по развёртыванию](deployment.md#staging-и-production-на-linux).

## 1. Конфигурация сервера

Для пилотного запуска с небольшой нагрузкой можно начать со следующей конфигурации:

| Параметр | Стартовая конфигурация |
|---|---|
| Процессор | 1 vCPU |
| Оперативная память | 2 ГБ |
| Накопитель | От 25 ГБ SSD или NVMe |
| Операционная система | Ubuntu Server 24.04 LTS, x86_64 / amd64 |
| Сеть | Публичный IPv4 и доступ к консоли VPS через панель провайдера |

Фактическую вместимость определяют частота и размер публикаций, QoS, число подписчиков и объём постоянных очередей. Подходящий профиль проверяют [нагрузочным измерением](benchmarking.md) на выбранном сервере. PostgreSQL и ОС используют ресурсы вместе с брокером.

При заказе задайте имя сервера, например `server.example.com`. Для первого подключения используйте административную учётную запись, выданную провайдером; далее примеры рассчитаны на `root`.

## 2. Примерные адреса

В командах используются демонстрационные значения:

| Значение | Назначение |
|---|---|
| `192.0.2.10` | Публичный IPv4 VPS |
| `server.example.com` | Имя самого сервера |
| `example.com` | Домен панели и адрес MQTT-сервера |
| `22222` | Выбранный SSH-порт |
| `~/.ssh/id_ed25519` | Пример пути к существующему закрытому ключу на клиентском компьютере |

Замените их своими значениями. Имя VPS и адрес панели могут различаться: сервер с именем `server.example.com` может обслуживать панель на `https://example.com`.

Число `22222` служит примером. Выберите свободный TCP-порт в диапазоне `1024-65535` и используйте одно выбранное значение в конфигурации SSH, правилах сетевого экрана и командах подключения. Смена порта уменьшает поток автоматических попыток подключения к стандартному порту `22`; аутентификация выполняется по SSH-ключу.

## 3. DNS домена

Для корневого домена добавьте или измените A-запись:

| Имя | Тип | Значение |
|---|---|---|
| `@` или `example.com` | `A` | `192.0.2.10` |

Если нужен адрес `www.example.com`, направьте его отдельную A-запись на тот же IP и настройте перенаправление на основной адрес панели. Для имени VPS можно также создать A-запись `server` с этим IP.

Используйте публичный IP из панели провайдера. Локальный адрес VPS предназначен для его внутренней сети. NS-записи сохраняют выбранного DNS-провайдера; MX и почтовые записи относятся к отдельной настройке почты.

Для существующего веб-сервиса переключайте DNS после подготовки нового сервера и выбора способа выпуска HTTPS-сертификата. Основной адрес панели должен совпадать с `CLEX_PUBLIC_URL`; в этом примере итоговое значение – `https://example.com`.

## 4. Проверка ключа сервера

В консоли VPS, открытой через панель провайдера, выполните:

```bash
ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub
```

Команда показывает SHA-256 отпечаток публичного ключа сервера. На клиентском компьютере откройте терминал и подключитесь:

```bash
ssh root@192.0.2.10
```

Сравните показанный SSH отпечаток с результатом из консоли VPS. При совпадении подтвердите ключ ответом `yes`, затем введите первоначальный пароль в запросе SSH. Если значения различаются, выясните причину через консоль или поддержку провайдера до продолжения подключения.

## 5. Добавление существующего SSH-ключа

На клиентском компьютере выберите свою пару ключей. В примерах это `~/.ssh/id_ed25519` и его публичная часть `~/.ssh/id_ed25519.pub`; для RSA-ключа укажите, например, `id_rsa` и `id_rsa.pub`.

Если пары ещё нет, создайте её командой `ssh-keygen -t ed25519`, выбрав свободное имя файла. Закрытая часть хранится на клиентском компьютере; на VPS добавляется публичная часть с расширением `.pub`.

На клиентском компьютере с установленным `ssh-copy-id` выполните:

```bash
ssh-copy-id -i ~/.ssh/id_ed25519.pub root@192.0.2.10
```

Введите первоначальный пароль VPS в запросе SSH. Затем откройте второе окно терминала и проверьте отдельное подключение по ключу:

```bash
ssh -i ~/.ssh/id_ed25519 \
  -o IdentitiesOnly=yes \
  -o PreferredAuthentications=publickey \
  -o ControlPath=none \
  root@192.0.2.10
```

`PreferredAuthentications=publickey` проверяет именно вход по ключу, а `ControlPath=none` создаёт новое соединение. Запрос кодовой фразы закрытого ключа, если она задана, относится к локальному ключу.

Сохраните рабочую сессию открытой до проверки следующего этапа.

## 6. Проверка SSH и сетевого экрана

Дальнейшие команды выполняются на VPS под `root`:

```bash
/usr/sbin/sshd -T | grep -E '^(port|pubkeyauthentication|passwordauthentication|kbdinteractiveauthentication|permitrootlogin) '
systemctl is-active ssh.socket
ufw status
```

Следующий этап рассчитан на активный `ssh.socket` в Ubuntu 24.04. При другом способе запуска адаптируйте команды управления службой по [документации Ubuntu](https://ubuntu.com/server/docs/how-to/security/openssh-server/).

При активном UFW заранее разрешите новый SSH-порт:

```bash
ufw allow 22222/tcp
```

Если провайдер использует внешний сетевой экран или группу безопасности, разрешите TCP-порт `22222` и там. Первоначальный SSH-порт остаётся доступным до успешной проверки нового.

## 7. Вход только по ключу и новый порт

Перед первым изменением сохраните исходные SSH-фрагменты в закрытом каталоге:

```bash
install -d -m 0700 /root/clex-mqtt-ssh-backup
cp -a /etc/ssh/sshd_config.d /root/clex-mqtt-ssh-backup/
```

Создайте отдельный конфигурационный файл:

```bash
cat > /etc/ssh/sshd_config.d/00-clex-access.conf <<'EOF'
Port 22
Port 22222
PubkeyAuthentication yes
AuthenticationMethods publickey
PasswordAuthentication no
KbdInteractiveAuthentication no
PermitRootLogin prohibit-password
EOF
```

Файл задаёт вход только по ключу. Для `root` сохраняется ключевая аутентификация. Порт `22` временно работает вместе с новым портом.

В стандартной конфигурации Ubuntu каталог `sshd_config.d` подключается в начале основного файла. Для большинства параметров действует первое найденное значение, поэтому раннее имя `00-clex-access.conf` позволяет задать настройки раньше типовых cloud-init-фрагментов. Итог проверяется через `sshd -T`.

Проверьте синтаксис и примените настройки:

```bash
/usr/sbin/sshd -t &&
systemctl daemon-reload &&
systemctl restart ssh.socket ssh.service
```

Оператор `&&` запускает следующий шаг после успешного завершения предыдущего. При ошибке проверки исправьте конфигурацию перед применением.

В Ubuntu 24.04 параметры порта читает генератор systemd, поэтому перед перезапуском выполняется `daemon-reload`. [Описание socket activation в Ubuntu](https://discourse.ubuntu.com/t/sshd-now-uses-socket-based-activation-ubuntu-22-10-and-later/30189).

В новом окне терминала клиентского компьютера проверьте подключение:

```bash
ssh -p 22222 -i ~/.ssh/id_ed25519 \
  -o IdentitiesOnly=yes \
  -o PreferredAuthentications=publickey \
  -o ControlPath=none \
  root@192.0.2.10
```

При запросе подтверждения ключа сервера сравните отпечаток с ранее проверенным значением. После успешного входа можно переходить к удалению временного порта.

## 8. Удаление временного порта и итоговая проверка

На VPS удалите строку временного порта из созданного файла:

```bash
sed -i '/^Port 22$/d' /etc/ssh/sshd_config.d/00-clex-access.conf

/usr/sbin/sshd -t &&
systemctl daemon-reload &&
systemctl restart ssh.socket ssh.service
```

Проверьте эффективную конфигурацию и реальные слушающие сокеты:

```bash
/usr/sbin/sshd -T | grep -E '^(port|authenticationmethods|passwordauthentication|kbdinteractiveauthentication|permitrootlogin) '
ss -lntp '( sport = :22 or sport = :22222 )'
```

Ожидаемые значения:

```text
port 22222
permitrootlogin without-password
passwordauthentication no
kbdinteractiveauthentication no
authenticationmethods publickey
```

`without-password` – отображаемое OpenSSH название режима `prohibit-password`: вход под `root` выполняется по ключу. В списке сокетов ожидается только порт `22222`.

Если порт `22` остаётся в выводе, проверьте остальные активные директивы `Port` в `sshd_config`, его фрагментах и настройках `ssh.socket`. Для `Port` допускается несколько значений, поэтому удаление одной строки может сохранять другой источник этого порта.

В ещё одном окне клиентского терминала повторите вход на `22222` по команде предыдущего этапа. После успешной проверки можно закрыть исходную сессию. При активном сетевом экране удалите прежнее разрешение порта `22` после проверки правил; новый порт должен сохранять разрешение.

Для восстановления доступа используйте оставленную SSH-сессию или консоль провайдера. До применения исправлений выполните `sshd -t`; после восстановления проверьте новое подключение.

## 9. Сетевой экран UFW

Пример рассчитан на отдельный VPS с CLEX MQTT. После проверки SSH на выбранном порту разрешите входящие подключения к SSH и будущей HTTPS-панели. На этом этапе используются TCP-порты `22222`, `80` и `443`.

Сначала просмотрите ранее добавленные правила:

```bash
ufw show added
```

Проверьте, что выбранный SSH-порт разрешён также во внешнем сетевом экране провайдера, если он используется. Сохраните текущую SSH-сессию открытой.

На VPS под `root` задайте правила и включите UFW:

```bash
ufw default deny incoming &&
ufw default allow outgoing &&
ufw allow 22222/tcp &&
ufw allow 80/tcp &&
ufw allow 443/tcp &&
ufw enable
```

В примере `22222` заменяется своим SSH-портом. На запрос `Proceed with operation (y|n)?` ответьте `y`. Правила разрешения добавляются до включения фильтрации.

После завершения команды отдельно проверьте результат:

```bash
ufw status verbose
```

Ожидаются `Status: active`, политика `deny (incoming)` и разрешения для выбранного SSH-порта, `80/tcp` и `443/tcp`. При включённом IPv6 проверьте также соответствующие правила `(v6)`. Ранее добавленные разрешения сохраняются; просмотрите их и удалите лишние правила по их точным номерам или значениям.

В новом окне терминала клиентского компьютера проверьте отдельное соединение:

```bash
ssh -p 22222 -i ~/.ssh/id_ed25519 \
  -o IdentitiesOnly=yes \
  -o PreferredAuthentications=publickey \
  -o ControlPath=none \
  root@192.0.2.10
```

После успешного входа можно закрыть исходную сессию. При потере нового доступа используйте оставленную сессию или консоль провайдера для проверки правил; команда `ufw disable` временно отключает фильтрацию на время исправления.

Для MQTT с TLS разрешение TCP-порта `8883` добавляется при настройке соответствующего слушателя брокера. HTTP-порт брокера `8085`, локальный MQTT-порт `1883` и PostgreSQL `5432` используют loopback согласно [production-конфигурации](deployment.md#staging-и-production-на-linux).

### Дополнительная защита SSH через Fail2ban

Fail2ban отслеживает неудачные SSH-входы и временно блокирует адреса источников. Его можно установить после настройки входа по ключу и UFW. Для Ubuntu 24.04 используйте journal systemd как источник событий и укажите фактический SSH-порт вместо примерного `22222`.

```bash
apt install -y fail2ban python3-systemd &&
cat > /etc/fail2ban/jail.d/sshd.local <<'JAIL'
[sshd]
enabled = true
backend = systemd
port = 22222
maxretry = 5
findtime = 10m
bantime = 1h
JAIL

fail2ban-client -t &&
systemctl enable fail2ban &&
systemctl restart fail2ban
```

Конфигурация задаёт блокировку на час после пяти учитываемых неудачных попыток за десять минут. Файл `jail.d/sshd.local` переопределяет параметры SSH, сохраняя пакетные настройки.

После запуска службы проверьте SSH jail:

```bash
fail2ban-client status sshd
```

Статус подтверждает загрузку jail; фактическое применение сетевой блокировки проверяется отдельно. Для проверки неудачных входов используйте отдельный тестовый адрес. [Параметры Fail2ban](https://github.com/fail2ban/fail2ban/blob/1.0.2/config/jail.conf).

## 10. Обновление Ubuntu и установка Nginx

Команды выполняются на новом VPS под `root`. Обновите пакеты текущей версии Ubuntu и установите Nginx:

```bash
apt update &&
apt upgrade -y &&
apt install -y nginx
```

Дождитесь завершения APT. Если менеджер пакетов предлагает заменить настроенный SSH-конфиг, сохраните текущую локальную версию (`keep the local version currently installed`).

Проверьте установленную версию, конфигурацию и состояние службы:

```bash
nginx -v
nginx -t
systemctl is-active nginx
```

Ожидаются успешная проверка конфигурации и состояние `active`. После установки Nginx по HTTP может открываться стандартная приветственная страница; маршруты панели и обработка неизвестных имён задаются на этапе настройки виртуального сервера.

Проверьте запрос Ubuntu на перезагрузку после обновления пакетов:

```bash
if [ -f /var/run/reboot-required ]; then
    cat /var/run/reboot-required
fi
```

Если появился запрос перезагрузки, сначала проверьте новое SSH-подключение по ключу и выбранному порту. Перед перезагрузкой подготовьте временный каталог для проверки SSH и включите автозапуск служб:

```bash
install -d -m 0755 -o root -g root /run/sshd &&
/usr/sbin/sshd -t &&
systemctl enable ssh.socket nginx ufw &&
reboot
```

Создание `/run/sshd` позволяет выполнить проверку при сообщении `Missing privilege separation directory: /run/sshd`. Каталог относится к временному состоянию службы; повторение этой ошибки после перезагрузки требует проверки `ssh.service` и его журнала через консоль провайдера. [Описание поведения проверки SSH на Ubuntu 24.04](https://checkmk.com/werk/17679).

После загрузки повторите подключение по ключу и выбранному порту. Проверьте `ufw status verbose`, `nginx -t` и `systemctl is-active ssh.socket nginx` перед дальнейшей настройкой.

Основной HTTPS-конфиг домена подключается после подготовки нужных путей и сертификатов. Настройка панели, HTTPS и MQTT WebSocket описана в [разделе HTTPS и WebSocket](deployment.md#https-и-websocket).

## 11. HTTP-конфигурация перед выпуском сертификата

На новом VPS создайте отдельный HTTP-сервер для домена. Каталог `/var/www/letsencrypt` используется для проверки HTTP-01 через Certbot webroot. Подставьте свои доменные имена вместо `example.com` и `www.example.com`.

```bash
install -d -m 0755 /var/www/letsencrypt/.well-known/acme-challenge
printf 'CLEX MQTT\n' > /var/www/letsencrypt/index.html

cat > /etc/nginx/sites-available/clex-mqtt <<'NGINX'
server {
    listen 80;
    listen [::]:80;
    server_name example.com www.example.com;
    root /var/www/letsencrypt;
}
NGINX

ln -s /etc/nginx/sites-available/clex-mqtt /etc/nginx/sites-enabled/clex-mqtt
nginx -t && systemctl reload nginx
```

Проверьте выбор виртуального сервера локально, независимо от DNS-кеша:

```bash
curl --fail --noproxy '*' --resolve example.com:80:127.0.0.1 http://example.com/
```

Ожидаемый ответ – `CLEX MQTT`. Эта проверка подтверждает локальную HTTP-конфигурацию; доступность из интернета и DNS проверяются отдельно перед запросом сертификата. Все имена в сертификате должны вести на этот VPS; проверьте также существующие записи AAAA. При недавней смене A-записей DNS-резолверы могут временно возвращать прежний адрес даже после очистки локального кеша.

Для последующего выпуска сертификата используйте `certbot certonly --webroot -w /var/www/letsencrypt` с нужными именами через `-d`. Сохраните обслуживание `/.well-known/acme-challenge/` из этого каталога в итоговой HTTP-конфигурации для автоматического продления. [Описание Certbot webroot](https://eff-certbot.readthedocs.io/en/stable/using.html#webroot).

## 12. Выпуск сертификата Let's Encrypt

Перед запросом проверьте публичные A/AAAA-записи всех имён сертификата и доступность HTTP-порта 80 из интернета. Ответ локального DNS-резолвера после недавней смены адреса может отличаться от публичных DNS-серверов.

Установите Certbot и запросите сертификат через подготовленный webroot:

```bash
apt install -y certbot &&
certbot certonly --webroot -w /var/www/letsencrypt \
    --cert-name example.com \
    -d example.com -d www.example.com
```

При интерактивном запросе укажите email для учётной записи ACME и ознакомьтесь с условиями Let's Encrypt. Имена из команды замените своими; `--cert-name` задаёт имя каталога сертификата.

После успешного выпуска файлы доступны по путям:

- `/etc/letsencrypt/live/example.com/fullchain.pem` – сертификат с цепочкой;
- `/etc/letsencrypt/live/example.com/privkey.pem` – закрытый ключ.

Затем подключите сертификат в HTTPS-конфигурации Nginx. Сохраните HTTP-обслуживание `/.well-known/acme-challenge/` из прежнего webroot и настройте применение обновлённых сертификатов службами при продлении. Закрытый ключ хранится на сервере с ограниченными правами и не включается в репозиторий. [Руководство Certbot](https://eff-certbot.readthedocs.io/en/stable/using.html#webroot).

## 13. HTTPS и основной адрес без www

После выпуска сертификата замените HTTP-конфигурацию следующим вариантом. Он сохраняет каталог HTTP-01 для продления, перенаправляет HTTP и HTTPS с `www` на основной адрес и обслуживает проверочную страницу по HTTPS. Пример использует сертификат с обоими именами.

```bash
cat > /etc/nginx/sites-available/clex-mqtt <<'NGINX'
server {
    listen 80;
    listen [::]:80;
    server_name example.com www.example.com;

    location ^~ /.well-known/acme-challenge/ {
        root /var/www/letsencrypt;
    }

    location / {
        return 301 https://example.com$request_uri;
    }
}

server {
    listen 443 ssl;
    listen [::]:443 ssl;
    server_name example.com www.example.com;

    ssl_certificate /etc/letsencrypt/live/example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/example.com/privkey.pem;
    ssl_protocols TLSv1.2 TLSv1.3;

    if ($host = www.example.com) {
        return 301 https://example.com$request_uri;
    }

    root /var/www/letsencrypt;
}
NGINX

nginx -t && systemctl reload nginx
```

После успешной перезагрузки выполните отдельный запрос с проверкой сертификата и SNI, независимо от DNS-кеша:

```bash
curl --fail --noproxy '*' --resolve example.com:443:127.0.0.1 https://example.com/
```

Ожидается ответ `CLEX MQTT`. Это проверка HTTPS со статической страницей. После запуска брокера замените обслуживание страницы проксированием панели и MQTT WebSocket из [production-шаблона](../deploy/nginx/clex-mqtt.conf), сохранив HTTP-01 и основной адрес. Для применения продлённых сертификатов настройте deploy hook Certbot с проверкой конфигурации и reload Nginx.

Синтаксис `listen 443 ssl` совместим с Nginx 1.24. [Настройка HTTPS в Nginx](https://nginx.org/en/docs/http/configuring_https_servers.html), [перенаправления return](https://nginx.org/en/docs/http/ngx_http_rewrite_module.html#return).

### Обработка неизвестных имён в default

На отдельном VPS замените стандартную страницу Nginx обработчиками по умолчанию. HTTP-запросы с неизвестным Host закрываются кодом Nginx 444, а TLS-подключения с неизвестным или отсутствующим SNI отклоняются во время handshake. Доменные серверы из предыдущего шага обслуживают свои имена отдельно.

Сохраните исходный файл и замените конфигурацию; пример использует существующую ссылку `sites-enabled/default`:

```bash
cp -a /etc/nginx/sites-available/default /etc/nginx/sites-available/default.before-clex
cat > /etc/nginx/sites-available/default <<'NGINX'
server {
    listen 80 default_server;
    listen [::]:80 default_server;
    server_tokens off;
    return 444;
}

server {
    listen 443 ssl default_server;
    listen [::]:443 ssl default_server;
    server_tokens off;
    ssl_protocols TLSv1.2 TLSv1.3;
    ssl_reject_handshake on;
}
NGINX

nginx -t && systemctl reload nginx
```

TLS-протоколы указаны и в сервере по умолчанию, поскольку часть TLS-настроек определяется до выбора доменного сервера. После применения повторите проверку HTTPS основного домена. [Выбор сервера Nginx](https://nginx.org/en/docs/http/request_processing.html), [ssl_reject_handshake](https://nginx.org/en/docs/http/ngx_http_ssl_module.html#ssl_reject_handshake).

## 14. Автопродление и перечитывание сертификата Nginx

После успешного продления Nginx должен перечитать сертификат. Создайте исполняемый deploy hook Certbot, который проверяет конфигурацию и выполняет reload:

```bash
install -d -m 0755 /etc/letsencrypt/renewal-hooks/deploy

cat > /etc/letsencrypt/renewal-hooks/deploy/20-reload-nginx <<'HOOK'
#!/bin/sh
set -eu
/usr/sbin/nginx -q -t
/usr/bin/systemctl reload nginx
HOOK

chmod 0755 /etc/letsencrypt/renewal-hooks/deploy/20-reload-nginx
systemctl enable --now certbot.timer
```

В hook флаг `-q` убирает штатные сообщения об успешной проверке конфигурации. Ошибки проверки остаются видны и останавливают выполнение до reload. [Параметры Nginx](https://nginx.org/en/docs/switches.html).

Для установки Certbot через APT в Ubuntu проверьте пробное продление и расписание таймера:

```bash
certbot renew --cert-name example.com --dry-run --run-deploy-hooks
systemctl list-timers certbot.timer --no-pager
```

Замените `example.com` именем сертификата. Ожидаются успешное пробное продление, выполнение hook без ошибок и следующий запуск `certbot.timer`. Флаг `--run-deploy-hooks` включает выполнение deploy hook после успешной проверки; hook использует действующий сертификат. Просмотрите вывод hook: ошибка hook может быть показана отдельно от результата продления.

При последующем подключении MQTT TLS настройте также обновление доступных брокеру копий сертификата и проверенный перезапуск брокера, как описано в [production-конфигурации](deployment.md#staging-и-production-на-linux). [Продление и hooks Certbot](https://eff-certbot.readthedocs.io/en/stable/using.html#renewing-certificates).

## 15. Установка PostgreSQL 17

Для PostgreSQL 17 на Ubuntu 24.04 подключите официальный APT-репозиторий PostgreSQL через скрипт из пакета `postgresql-common`. Команды выполняются под `root` на отдельном VPS:

```bash
apt install -y postgresql-common &&
/usr/share/postgresql-common/pgdg/apt.postgresql.org.sh &&
apt update &&
apt install -y postgresql-17
```

Если скрипт запросит подтверждение подключения репозитория, подтвердите его. Имя пакета `postgresql-17` явно выбирает основную версию 17.

После завершения установки проверьте кластеры:

```bash
pg_lsclusters
```

Ожидается кластер версии `17` со статусом `online`. Перед созданием роли и базы брокера проверьте адреса прослушивания, аутентификацию и параметры надёжной записи:

```bash
ss -lntp '( sport = :5432 )'

runuser -l postgres -c 'psql -X -v ON_ERROR_STOP=1 -d postgres -P pager=off' <<'SQL'
SELECT name, setting
FROM pg_settings
WHERE name IN ('listen_addresses', 'port', 'password_encryption',
               'fsync', 'full_page_writes', 'synchronous_commit')
ORDER BY name;

SELECT type, database, user_name, address, auth_method
FROM pg_hba_file_rules
ORDER BY rule_number;
SQL
```

Для локального размещения ожидаются loopback-адреса PostgreSQL, `listen_addresses=localhost`, `password_encryption=scram-sha-256` и значения `on` для `fsync`, `full_page_writes`, `synchronous_commit`. Правила TCP для loopback должны использовать `scram-sha-256`; локальный административный доступ через Unix socket может использовать `peer`.

Второй запрос показывает содержимое файла правил аутентификации. После его изменения требуется reload и отдельная проверка нового соединения. PostgreSQL используется локально; доступ к панели и MQTT проходит через отдельные слушатели. [Параметры pg_settings](https://www.postgresql.org/docs/17/view-pg-settings.html), [правила pg_hba_file_rules](https://www.postgresql.org/docs/17/view-pg-hba-file-rules.html).

[Установка PostgreSQL в Ubuntu и официальный APT-репозиторий](https://www.postgresql.org/download/linux/ubuntu/).

## 16. Отдельная роль и база брокера

На новом сервере создайте роль `clex_mqtt` и одноимённую базу. Роль владеет своей базой и получает права для применения миграций и штатной работы брокера. Создание других баз, управление ролями, репликация, обход RLS и права суперпользователя выключены.

```bash
runuser -l postgres -c 'psql -X -v ON_ERROR_STOP=1 -d postgres' <<'SQL'
CREATE ROLE clex_mqtt LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS;
CREATE DATABASE clex_mqtt OWNER clex_mqtt TEMPLATE template0 ENCODING 'UTF8';
REVOKE ALL ON DATABASE clex_mqtt FROM PUBLIC;
SQL
```

После успешного создания выполните отдельной командой установку пароля:

```bash
runuser -l postgres -c 'psql -X -v ON_ERROR_STOP=1 -d postgres -c "\password clex_mqtt"'
```

Введите отдельный пароль БД дважды и сохраните его в защищённом хранилище для дальнейшей настройки сервиса. Для назначения пароля используется интерактивная команда psql `\password`. После первого блока ожидаются ответы `CREATE ROLE`, `CREATE DATABASE` и `REVOKE`.

Затем проверьте новое TCP-соединение с ролью брокера:

```bash
psql -X -h 127.0.0.1 -p 5432 -U clex_mqtt -d clex_mqtt \
    -W -v ON_ERROR_STOP=1 -P pager=off \
    -c 'SELECT current_user, current_database();'
```

Ожидается `clex_mqtt` в обоих столбцах. Таблицы брокера создаются позднее отдельной командой `clex-mqtt migrate` из собранного Linux-бинарника с включёнными миграциями.

[Роли PostgreSQL](https://www.postgresql.org/docs/17/sql-createrole.html), [создание базы](https://www.postgresql.org/docs/17/sql-createdatabase.html), [команды psql](https://www.postgresql.org/docs/17/app-psql.html).

## 17. Системные пакеты и ресурсы для сборки

Установите системные инструменты на VPS:

```bash
apt install -y build-essential pkg-config curl ca-certificates xz-utils python3
```

Перед установкой Rust и Node.js проверьте архитектуру, доступную память, swap и свободное место:

```bash
uname -m
free -h
swapon --show
df -h /
```

Архитектура определяет выбор установочного архива Node.js; сведения о памяти и диске нужны для выбора параметров сборки. Версия Rust закреплена в `rust-toolchain.toml`; требования Node.js указаны в [руководстве развёртывания](deployment.md#staging-и-production-на-linux). Пакет `build-essential` предоставляет C-компилятор и линкер для Rust и зависимостей с C-кодом. [Установка Rust](https://doc.rust-lang.org/book/ch01-01-installation.html).

### Файл подкачки перед сборкой

На VPS с небольшим объёмом RAM файл подкачки даёт дополнительный запас памяти на время компиляции. Размер выбирается по доступному месту и нагрузке; следующий пример создаёт 2 ГиБ swap на локальной ext4 или XFS.

```bash
dd if=/dev/zero of=/swapfile bs=1M count=2048 conv=excl status=progress &&
chmod 600 /swapfile &&
mkswap /swapfile &&
swapon /swapfile &&
{ grep -qE '^[[:space:]]*/swapfile[[:space:]]' /etc/fstab ||
    printf '/swapfile none swap sw,nofail 0 0\n' >> /etc/fstab; }

swapon --show
```

Ожидается активный `/swapfile` размером 2 ГиБ. `conv=excl` предотвращает перезапись существующего файла; запись в `fstab` добавляется после успешной активации. На файловых системах с copy-on-write требуется отдельная подготовка swap-файла согласно их документации. Для ограниченного объёма памяти используйте один процесс компиляции Cargo: `cargo build --release --locked -j 1`, предварительно собрав frontend. [Работа со swap-файлами в Ubuntu](https://manpages.ubuntu.com/manpages/noble/en/man8/swapon.8.html).

## 18. Установка Rust

Установите Rust версии, закреплённой в `rust-toolchain.toml`. В следующем примере используются Rust 1.93.1, профиль `minimal` и компоненты `rustfmt`, `clippy` из конфигурации проекта. Команды выполняются на VPS под пользователем сборки; пример рассчитан на `root`:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs -o /root/clex-rustup-init.sh &&
sh /root/clex-rustup-init.sh -y --profile minimal \
    --default-toolchain 1.93.1 --component rustfmt --component clippy &&
. "$HOME/.cargo/env" &&
rustc --version &&
cargo --version
```

Ожидается `rustc 1.93.1` и доступная команда Cargo. Rustup размещает инструменты в каталоге пользователя сборки. Рабочая служба брокера далее запускается отдельным непривилегированным пользователем согласно systemd-шаблону.

[Установка rustup](https://rust-lang.github.io/rustup/installation/index.html), [профили установки](https://rust-lang.github.io/rustup/concepts/profiles.html).

## 19. Установка Node.js для Linux x64

Для сборки панели установите Node.js 22.21.1, соответствующий версии в `deploy/Dockerfile`. Следующий пример рассчитан на архитектуру `x86_64` из проверки `uname -m` и установку под `root`. Контрольная сумма взята из официального выпуска Node.js.

```bash
cd /root &&
curl --fail --location https://nodejs.org/dist/v22.21.1/node-v22.21.1-linux-x64.tar.xz \
    -o node-v22.21.1-linux-x64.tar.xz &&
printf '680d3f30b24a7ff24b98db5e96f294c0070f8f9078df658da1bce1b9c9873c88  node-v22.21.1-linux-x64.tar.xz\n' | sha256sum -c - &&
tar --no-same-owner -xJf node-v22.21.1-linux-x64.tar.xz -C /usr/local --strip-components=1 &&
hash -r &&
node --version &&
npm --version
```

Ожидаются успешная проверка SHA-256, версия Node.js `v22.21.1` и доступный npm. Для другой архитектуры выберите соответствующий архив и его контрольную сумму на странице выпуска. Node.js используется при сборке панели; работающий брокер обслуживает встроенные файлы интерфейса.

[Официальный выпуск Node.js 22.21.1 и SHA-256](https://nodejs.org/en/blog/release/v22.21.1).

## 20. Подготовка и перенос исходников

На локальном компьютере в корне проекта создайте отдельный каталог для переноса. Следующий пример копирует текущие файлы, учитываемые Git, включая неигнорируемые новые файлы. Он проверяет кандидатов на известные секреты и исключает служебные инструкции, runtime-конфигурацию и закрытые ключи.

Каталог назначения должен быть новым. Копируются содержимое файлов и права доступа, без метаданных Git и расширенных атрибутов файловой системы.

```bash
python3 scripts/check_secrets.py &&
python3 - <<'PY'
from pathlib import Path
import os
import shutil
import subprocess

root = Path.cwd()
dest = root / ".cache" / "clex-mqtt-src"
if dest.exists() or dest.is_symlink():
    raise SystemExit("Export directory already exists; choose a new empty destination.")
raw = subprocess.check_output(
    ["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"],
    cwd=root, stderr=subprocess.PIPE,
)
paths = sorted({os.fsdecode(item) for item in raw.split(b"\0") if item})
examples = {".env.example", ".env.production.example"}
for name in paths:
    rel = Path(name)
    source = root / rel
    forbidden = (
        rel.is_absolute() or ".." in rel.parts
        or source.is_symlink() or not source.is_file()
        or "AGENTS.md" in rel.parts or ".git" in rel.parts
        or (rel.name.startswith(".env") and rel.name not in examples)
        or rel.suffix.lower() in {".pem", ".key", ".p12", ".pfx", ".pgpass", ".dump", ".log"}
    )
    if forbidden:
        raise SystemExit("Unsupported or private export candidate: " + name)
dest.mkdir(parents=True)
for name in paths:
    source = root / name
    target = dest / name
    target.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(source, target)
    target.chmod(source.stat().st_mode & 0o777)
print("Prepared", len(paths), "files in", dest)
PY
```

С локального компьютера отправьте подготовленный каталог на VPS. Замените примерный SSH-порт и IP своими:

```bash
scp -P 22222 -r .cache/clex-mqtt-src root@192.0.2.10:/opt/
```

Исходники окажутся в `/opt/clex-mqtt-src`. Здесь выполняется Linux-сборка; исполняемый файл рабочей службы далее устанавливается в `/opt/clex-mqtt` по production-инструкции. Используйте отдельную production-конфигурацию сервиса.

## 21. Сборка панели и брокера на VPS

Выполните сборку в перенесённом каталоге исходников. Сначала установите зависимости из npm lock-файла и соберите frontend, затем соберите Rust-бинарник со встроенной панелью и миграциями. Для VPS с ограниченной памятью используйте один процесс компиляции Cargo:

```bash
cd /opt/clex-mqtt-src &&
. "$HOME/.cargo/env" &&
cc --version &&
npm ci &&
npm run ui:build &&
cargo build --release --locked -j 1
```

Команды соединены через `&&`, поэтому следующий этап выполняется только после успешного предыдущего. Готовый Linux-бинарник находится в `/opt/clex-mqtt-src/target/release/clex-mqtt`. После сборки установите его в каталог службы, подготовьте закрытую production-конфигурацию и примените встроенные миграции по [руководству развёртывания](deployment.md#staging-и-production-на-linux).

### Повторные сборки

Cargo повторно использует подходящие артефакты зависимостей из `target`. Сохраняйте `/opt/clex-mqtt-src/target` и каталог кеша Cargo между обновлениями исходников. Изменения Rust-кода обычно требуют пересборки проекта и затронутых зависимостей; смена toolchain, параметров сборки или версий зависимостей может увеличить объём работы.

В release-профиле проекта заданы `lto = "thin"` и `codegen-units = 1`. Оптимизация при линковке увеличивает длительность сборки, поэтому небольшая правка не гарантирует мгновенную пересборку. Панель включена в исполняемый файл брокера: после изменений frontend требуется собрать панель и затем бинарник. Точное время оценивается по конкретной повторной сборке.

[Кеш Cargo](https://doc.rust-lang.org/cargo/reference/build-cache.html), [профили и LTO](https://doc.rust-lang.org/cargo/reference/profiles.html).

## 22. Установка бинарника и шаблона службы

После успешной Linux-сборки создайте отдельного системного пользователя и установите бинарник, документацию и unit systemd. Следующий блок предназначен для первой установки, когда пользователь `clex-mqtt` ещё не существует:

```bash
useradd --system --user-group --home-dir /nonexistent --no-create-home --shell /usr/sbin/nologin clex-mqtt &&
install -d -m 0755 /opt/clex-mqtt /opt/clex-mqtt/docs &&
install -d -m 0750 -o root -g clex-mqtt /etc/clex-mqtt /etc/clex-mqtt/certs &&
install -m 0755 /opt/clex-mqtt-src/target/release/clex-mqtt /opt/clex-mqtt/clex-mqtt &&
install -m 0644 /opt/clex-mqtt-src/docs/deployment.md /opt/clex-mqtt/docs/deployment.md &&
install -m 0644 /opt/clex-mqtt-src/deploy/systemd/clex-mqtt.service /etc/systemd/system/clex-mqtt.service &&
systemctl daemon-reload &&
/opt/clex-mqtt/clex-mqtt --version
```

Ожидается версия собранного брокера. Далее подготовьте закрытый EnvironmentFile, файлы TLS, примените миграции и создайте администратора. После этого включите и запустите службу по [production-инструкции](deployment.md#staging-и-production-на-linux).

## 23. Закрытая production-конфигурация

На VPS под `root` создайте EnvironmentFile из production-шаблона. Подставьте основной домен вместо `https://example.com`. В интерактивном запросе введите существующий пароль роли PostgreSQL `clex_mqtt`.

Следующий код кодирует специальные символы пароля для DATABASE_URL, создаёт постоянный 32-байтный ключ мостов и случайный токен метрик, затем записывает файл с правами `0600`. Значения секретов сохраняются в файле; в выводе показывается только путь.

```bash
python3 - <<'PY'
import base64
import getpass
import os
import secrets
import warnings
from pathlib import Path
from urllib.parse import quote

path = Path("/etc/clex-mqtt/clex-mqtt.env")
if path.exists() or path.is_symlink():
    raise SystemExit("Конфигурация уже существует; файл сохранён.")

text = Path("/opt/clex-mqtt-src/.env.production.example").read_text()
placeholder = "postgresql://USER:PASSWORD@127.0.0.1:5432/clex_mqtt"
if text.count(placeholder) != 1 or text.count("https://mqtt.example.com") != 1:
    raise SystemExit("Проверьте шаблон production-конфигурации.")

warnings.simplefilter("error", getpass.GetPassWarning)
password = getpass.getpass("Пароль БД для clex_mqtt: ")
if not password:
    raise SystemExit("Пароль БД не может быть пустым.")
encoded = quote(password, safe="")
text = text.replace(placeholder, f"postgresql://clex_mqtt:{encoded}@127.0.0.1:5432/clex_mqtt")
text = text.replace("https://mqtt.example.com", "https://example.com")
text += "\nCLEX_BRIDGE_SECRET_KEY=" + base64.b64encode(secrets.token_bytes(32)).decode()
text += "\nCLEX_METRICS_TOKEN=" + secrets.token_urlsafe(32) + "\n"

fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
with os.fdopen(fd, "w") as output:
    output.write(text)
    output.flush()
    os.fsync(output.fileno())
print(f"Создан {path} (0600).")
PY
```

Production-шаблон задаёт `CLEX_HTTP_TRUSTED_PROXY_IPS=127.0.0.1,::1` для локального Nginx. Итоговый прокси должен перезаписывать `X-Real-IP` адресом посетителя, как в конфигурации ниже. В существующей установке добавьте параметр явно при обновлении; подробности ограничений и журнала – в [разделе защиты входа](deployment.md#защита-входа-и-журнал).

Ожидается сообщение о создании `/etc/clex-mqtt/clex-mqtt.env`. Существующий файл сохраняется при повторном запуске. Ключ мостов должен сохраняться при обновлении сервиса и входить в защищённую резервную копию конфигурации вместе с БД согласно [правилам хранения ключа](deployment.md#ключ-и-подключения-мостов).

Для последующей настройки MQTT TLS подготовьте доступные пользователю брокера копии сертификата и закрытого ключа в `/etc/clex-mqtt/certs`, затем настройте их обновление при продлении сертификата. [Скрытый ввод getpass](https://docs.python.org/3.12/library/getpass.html), [кодирование URL](https://docs.python.org/3.12/library/urllib.parse.html#urllib.parse.quote).

## 24. Сертификат MQTT TLS и обновление при продлении

Системный пользователь брокера читает отдельные копии серверного сертификата и закрытого ключа из `/etc/clex-mqtt/certs`. Подставьте имя сертификата вместо `example.com` и создайте deploy hook Certbot:

```bash
cat > /etc/letsencrypt/renewal-hooks/deploy/30-clex-mqtt-tls <<'HOOK'
#!/bin/sh
set -eu
[ "${RENEWED_LINEAGE:-}" = /etc/letsencrypt/live/example.com ] || exit 0
/usr/bin/install -o root -g clex-mqtt -m 0640 "$RENEWED_LINEAGE/fullchain.pem" /etc/clex-mqtt/certs/fullchain.pem
/usr/bin/install -o root -g clex-mqtt -m 0640 "$RENEWED_LINEAGE/privkey.pem" /etc/clex-mqtt/certs/privkey.pem
if /usr/bin/systemctl is-active --quiet clex-mqtt.service; then
    /usr/bin/systemctl restart clex-mqtt.service
fi
HOOK

chmod 0755 /etc/letsencrypt/renewal-hooks/deploy/30-clex-mqtt-tls &&
RENEWED_LINEAGE=/etc/letsencrypt/live/example.com /etc/letsencrypt/renewal-hooks/deploy/30-clex-mqtt-tls &&
runuser -u clex-mqtt -- test -r /etc/clex-mqtt/certs/fullchain.pem &&
runuser -u clex-mqtt -- test -r /etc/clex-mqtt/certs/privkey.pem &&
stat -c '%a %U:%G %n' /etc/clex-mqtt/certs/fullchain.pem /etc/clex-mqtt/certs/privkey.pem
```

Ожидаются две строки с правами `640` и владельцем `root:clex-mqtt`. Перед выводом метаданных проверяется доступ к обоим файлам от пользователя службы; содержимое закрытого ключа остаётся в файле.

Первый явный запуск hook подготавливает копии сертификата. После успешных продлений Certbot передаёт `RENEWED_LINEAGE`; hook обрабатывает выбранный сертификат, обновляет файлы и перезапускает уже работающую службу. Брокер загружает TLS-конфигурацию при старте, поэтому для применения обновления требуется перезапуск. MQTT-клиентам требуется переподключение после перезапуска.

Hook Nginx из предыдущего этапа продолжает перечитывать сертификат панели. Затем примените встроенные миграции и создайте администратора до первого запуска службы. [Deploy hooks Certbot](https://eff-certbot.readthedocs.io/en/stable/using.html#renewing-certificates).

## 25. Применение встроенных миграций

До первого запуска службы примените SQL-миграции из собранного бинарника. Команда запускается от системного пользователя брокера и получает production-конфигурацию через systemd EnvironmentFile:

```bash
systemd-run --quiet --wait --pipe --collect \
    --property=User=clex-mqtt --property=Group=clex-mqtt \
    --property=EnvironmentFile=/etc/clex-mqtt/clex-mqtt.env \
    --property=WorkingDirectory=/opt/clex-mqtt \
    /opt/clex-mqtt/clex-mqtt migrate
```

Ожидается сообщение `Database migrations applied`. Содержимое файла окружения остаётся закрытым. При обновлении существующей установки сначала остановите брокер по [порядку обновления и резервного копирования](deployment.md#обновление-резервная-копия-и-откат); команда миграций дополнительно проверяет блокировку экземпляра.

После успешного применения создайте администратора панели, затем запустите службу.

## 26. Создание администратора панели

Создайте администратора с именем `admin` и отдельным паролем панели. Команда запрашивает пароль дважды со скрытым вводом и передаёт его бинарнику через stdin. Допустимый размер пароля – 12-256 байт UTF-8.

```bash
python3 - <<'PY'
import getpass
import subprocess
import warnings

warnings.simplefilter("error", getpass.GetPassWarning)
password = getpass.getpass("Новый пароль панели для admin: ")
confirmation = getpass.getpass("Повтори пароль панели: ")
if password != confirmation:
    raise SystemExit("Пароли не совпадают.")
if not 12 <= len(password.encode("utf-8")) <= 256:
    raise SystemExit("Пароль должен содержать 12-256 байт UTF-8.")

command = [
    "systemd-run", "--quiet", "--wait", "--pipe", "--collect",
    "--property=User=clex-mqtt", "--property=Group=clex-mqtt",
    "--property=EnvironmentFile=/etc/clex-mqtt/clex-mqtt.env",
    "--property=WorkingDirectory=/opt/clex-mqtt",
    "/opt/clex-mqtt/clex-mqtt", "admin", "create",
    "--username", "admin", "--password-stdin",
]
result = subprocess.run(command, input=password + "\n", text=True)
raise SystemExit(result.returncode)
PY
```

Ожидается сообщение `Panel administrator created`. Пароль хранится в PostgreSQL как хеш Argon2. При повторном создании того же имени команда сообщает о существующем администраторе и сохраняет его прежний пароль. Учётные записи панели и MQTT-клиентов разделены.

После успешного создания включите службу, проверьте readiness и настройте Nginx для проксирования панели и MQTT WebSocket.

## 27. Первый запуск службы

Для локального PostgreSQL добавьте зависимость запуска от фактического кластера. В примере используется `17/main`, соответствующий unit `postgresql@17-main.service`; при другом имени кластера подставьте его unit.

```bash
systemctl is-active postgresql@17-main.service &&
install -d -m 0755 /etc/systemd/system/clex-mqtt.service.d &&
printf '[Unit]\nWants=postgresql@17-main.service\nAfter=postgresql@17-main.service\n' \
    > /etc/systemd/system/clex-mqtt.service.d/postgresql.conf &&
systemctl daemon-reload &&
systemctl enable --now clex-mqtt
```

После запуска выполните отдельно проверку состояния и готовности:

```bash
systemctl status clex-mqtt --no-pager --full
curl --fail --silent --show-error http://127.0.0.1:8085/health/ready
```

Ожидаются состояние `active (running)` и ответ `{"status":"ready"}`. Проверка готовности выполняет запрос к PostgreSQL; она подтверждает локальную готовность HTTP и БД. Публичное HTTPS-проксирование и MQTT TLS проверяются следующими этапами. `Wants` и `After` задают запуск локального кластера и порядок старта, сохраняя основной шаблон службы. [Зависимости и drop-in systemd](https://manpages.ubuntu.com/manpages/noble/en/man5/systemd.unit.5.html).

## 28. Проксирование панели и MQTT WebSocket

После успешной локальной проверки брокера подготовьте доменный конфиг Nginx. Замените примерные имена и пути сертификата своими. Сохраните файл на VPS как `/root/clex-mqtt-nginx.conf`:

```nginx
server {
    listen 80;
    listen [::]:80;
    server_name example.com www.example.com;
    server_tokens off;

    location ^~ /.well-known/acme-challenge/ {
        root /var/www/letsencrypt;
    }

    location / {
        return 301 https://example.com$request_uri;
    }
}

server {
    listen 443 ssl;
    listen [::]:443 ssl;
    server_name example.com www.example.com;

    ssl_certificate /etc/letsencrypt/live/example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/example.com/privkey.pem;
    ssl_protocols TLSv1.2 TLSv1.3;
    server_tokens off;
    client_max_body_size 16k;

    if ($host = www.example.com) {
        return 301 https://example.com$request_uri;
    }

    location = /metrics {
        return 404;
    }

    location = /mqtt {
        proxy_pass http://127.0.0.1:8085;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_connect_timeout 5s;
        proxy_read_timeout 86400s;
        proxy_send_timeout 30s;
        proxy_buffering off;
    }

    location / {
        proxy_pass http://127.0.0.1:8085;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_connect_timeout 5s;
        proxy_read_timeout 30s;
        proxy_send_timeout 30s;
    }
}
```

Конфигурация сохраняет HTTP-01, основной адрес без `www`, закрывает публичный `/metrics` и направляет панель и MQTT WebSocket на HTTP-слушатель брокера. Защитный default из предыдущего этапа продолжает обслуживать неизвестные имена.

Сохраните предыдущий доменный конфиг, установите подготовленный файл и примените его после проверки:

```bash
cp -a /etc/nginx/sites-available/clex-mqtt /etc/nginx/sites-available/clex-mqtt.before-proxy &&
install -m 0644 /root/clex-mqtt-nginx.conf /etc/nginx/sites-available/clex-mqtt &&
nginx -t &&
systemctl reload nginx
```

После reload выполните отдельную проверку через HTTPS с правильным Host и SNI:

```bash
curl --fail --silent --show-error --noproxy '*' \
    --resolve example.com:443:127.0.0.1 https://example.com/health/ready
```

Ожидается `{"status":"ready"}`. Затем откройте основной адрес панели в браузере и войдите с созданными административными учётными данными. Для MQTT WebSocket используется адрес `wss://example.com/mqtt` с subprotocol `mqtt` и отдельными MQTT-учётными данными. [WebSocket proxying Nginx](https://nginx.org/en/docs/http/websocket.html).

## 29. Публичный MQTT TLS-порт

После успешного запуска TLS-слушателя проверьте его адрес и добавьте разрешение UFW для нативного MQTT TLS:

```bash
ss -lntp '( sport = :8883 )'
ufw allow 8883/tcp
ufw status
```

При `CLEX_TLS_ADDR=0.0.0.0:8883` ожидается слушатель на IPv4-порту `8883` и разрешение `8883/tcp` в UFW. Локальные PostgreSQL, HTTP и plaintext MQTT продолжают использовать loopback.

С другого компьютера можно проверить TLS, доверие к цепочке и имя сертификата через Python 3. Замените примерное имя своим:

```bash
python3 - <<'PY'
import socket
import ssl

host = "example.com"
context = ssl.create_default_context()
with socket.create_connection((host, 8883), timeout=10) as raw:
    with context.wrap_socket(raw, server_hostname=host) as tls:
        print(tls.version(), "certificate and hostname verified")
PY
```

Для MQTT-клиента используйте имя сервера и порт `8883` с проверкой сертификата. Вход выполняется отдельной MQTT-учётной записью с заданными правилами публикации и подписки; административная учётная запись панели относится к другому механизму доступа.

Проверка TLS подтверждает транспортное соединение и сертификат. Обмен сообщениями и подтверждения MQTT проверяются отдельно согласно [описанию возможностей](features.md) и сценарию эксплуатации.

## 30. Проверка продления после завершения настройки

После установки финального Nginx-конфига и MQTT deploy hook выполните пробное продление с обоими hooks. MQTT hook перезапускает работающий брокер, поэтому проверка вызывает переподключение клиентов; выполняйте её до подключения постоянных клиентов или в выбранное время обслуживания.

```bash
systemctl show clex-mqtt --property=MainPID
certbot renew --cert-name example.com --dry-run --run-deploy-hooks
systemctl show clex-mqtt --property=MainPID --property=ActiveState
```

Замените `example.com` именем сертификата. Ожидаются успешное пробное продление, смена `MainPID` брокера и состояние `ActiveState=active`. Штатная успешная проверка Nginx в hook выполняется тихо благодаря `nginx -q -t`; ошибки hooks требуют проверки отдельно от сводки продления.

После завершения отдельно проверьте готовность HTTPS и TLS-подключение на порту `8883` по предыдущим этапам. Эта последовательность проверяет HTTP-01 через финальный конфиг, обновление копий сертификата и перезапуск службы. [Проверка продления и deploy hooks Certbot](https://eff-certbot.readthedocs.io/en/stable/using.html#renewing-certificates).

## 31. Первый MQTT-пользователь через панель

Войдите в административную панель и откройте «MQTT-доступ» → «Учётные записи» → «Добавить учётную запись». Заполните форму вручную. Пример отдельной учётной записи устройства:

| Поле | Значение |
|---|---|
| Имя пользователя | `device-01` |
| Пароль | Отдельный пароль устройства от 12 символов |
| Публикация | `devices/device-01/state` |
| Подписка | `devices/device-01/commands/#` |

Для этого примера достаточно личных разрешений. Нажмите «Создать пользователя». Устройство сможет публиковать состояние в свой топик и подписываться на команды в своём пространстве имён. При нескольких фильтрах каждый вводится с новой строки.

Во вкладке «Проверка доступа» выберите пользователя `device-01`, действие «Публикация» и топик `devices/device-01/state`: ожидается разрешение. Для `devices/device-02/state` ожидается запрет при отсутствии дополнительных разрешений. Личные разрешения дополняют активные группы; явные запреты имеют приоритет.

В MQTT-клиенте задайте свой домен, порт `8883`, TLS с проверкой сертификата, имя `device-01` и сохранённый пароль. Client ID, например `device-01`, идентифицирует подключение в рамках MQTT-учётной записи. Пароль администратора панели относится к отдельной системе входа.

Для проверки публикаций откройте «Сообщения» → «Поток». Укажите в клиенте конкретный топик `devices/device-01/state`. Фильтр подписки `devices/device-01/commands/#` применяется для получения команд от другого MQTT-пользователя с правом публикации в эти топики.

## 32. Локальные резервные копии

После проверки брокера можно установить [скрипт и таймер резервного копирования](backups.md). Они сохраняют семь завершённых комплектов БД, конфигурации и бинарника в `/var/backups/clex-mqtt`. Новая копия проверяется до удаления самой старой. Расписание и установка описаны в руководстве; остановка брокера для штатного копирования не требуется. После создания копии выполните [пробное восстановление](backups.md#пробное-восстановление) и отдельно проверьте запуск приложения с восстановленными данными.

## 33. Мониторинг и уведомления

Сбор показателей ОС и брокера, приватные адреса служб и доставка уведомлений описаны в [руководстве мониторинга](monitoring.md#локальный-стек-ubuntu-2404). Проверяйте конфигурации до запуска и подтверждайте доставку тестовым событием. Для локальных копий отдельно подключите [контроль результата и свежести](monitoring.md#контроль-резервных-копий).

## 34. Проверка нагрузки на отдельном экземпляре

Используйте выделенную тестовую БД с отдельной непривилегированной ролью и закрытым файлом `TEST_DATABASE_URL`. Рабочая БД не должна участвовать в стенде. Подготовьте копию установленного release-бинарника внутри каталога исходников и передайте её через `--binary`, чтобы измерять ту же сборку.

Начните с [равномерной подачи](benchmarking.md#равномерная-подача), затем выполните несколько одинаковых пакетных прогонов и постепенно увеличивайте число отправителей и получателей. Проверяйте счётчики каждого подписчика, задержки, учёт очередей и состояние рабочего сервиса. На общем VPS процессы стенда, PostgreSQL и рабочий брокер используют одни CPU и диск; изоляция БД не изолирует аппаратные ресурсы.

Результаты сохраняются в игнорируемом `work/benchmarks`. После завершения остановите только тестовые процессы, удалите созданные тестовую БД и роль, закрытый файл с временными учётными данными и копию тестового бинарника. Сохраните отчёты для сравнения. Короткие loopback-прогоны проверяют выбранные профили; длительная работа с реальной сетью, TLS и целевым числом устройств требует отдельного испытания.

## 35. Проверка автозапуска после перезагрузки

Перезагрузка прерывает подключения MQTT и SSH. Выполняйте её в выбранное время обслуживания, сохранив доступ к консоли провайдера. Сначала проверьте конфигурации и включение основных служб:

```bash
/usr/sbin/sshd -t
nginx -q -t
systemctl is-enabled ssh.socket nginx.service ufw.service postgresql.service clex-mqtt.service
systemctl show clex-mqtt.service --property=Wants --property=After
cat /proc/sys/kernel/random/boot_id
```

Для штатного кластера `17/main` проверьте автоматический запуск в `/etc/postgresql/17/main/start.conf` и зависимость брокера от `postgresql@17-main.service`. Если установлены Fail2ban, мониторинг и таймеры копий, отдельно проверьте включение их служб и таймеров. Убедитесь, что сохранённое правило UFW разрешает используемый SSH-порт.

После успешных проверок выполните `systemctl reboot`. Повторно подключитесь по SSH и проверьте смену boot ID, состояние служб и готовность:

```bash
cat /proc/sys/kernel/random/boot_id
systemctl is-active ssh.socket nginx.service ufw.service postgresql@17-main.service clex-mqtt.service
curl --fail --silent --show-error http://127.0.0.1:8085/health/ready
systemctl show clex-mqtt.service --property=MainPID --property=NRestarts
systemctl --failed --no-pager
```

С отдельного компьютера проверьте публичный HTTPS и MQTT TLS с проверкой сертификата. В конфигурации мониторинга все три источника должны снова перейти в `up`, а правила должны выполняться без ошибок. Дождитесь обновления состояния копий после загрузки; минутный таймер запускается через минуту после старта системы. Проверьте расписание Certbot и копирования. Контрольный запуск `clex-mqtt-backup.service` проверяет создание новой копии в новом сеансе системы без перезапуска брокера.

Проверьте автоматическое переподключение устройств, которые были подключены до перезагрузки. Если таких клиентов не было, этот сценарий проверяется отдельно при работающем устройстве.

## Grafana

Для просмотра графиков подключите Grafana к уже работающему локальному Prometheus по [руководству мониторинга](monitoring.md#grafana). Grafana обслуживается по HTTPS-префиксу `/grafana/`, использует отдельный вход и включается в существующие резервные копии.

## Подключение устройств

Проверьте вход в панель и настройте MQTT-учётные записи и ACL для устройств. Обновление и резервное копирование описаны в [руководстве production-развёртывания](deployment.md#обновление-резервная-копия-и-откат). Nginx обслуживает панель на HTTPS-порту `443`, а MQTT с TLS – отдельный порт брокера `8883`. При настройке сетевого экрана разрешайте порты в соответствии с используемыми сервисами.

## Источники

- [Настройка OpenSSH в Ubuntu Server](https://ubuntu.com/server/docs/how-to/security/openssh-server/)
- [Параметры sshd_config](https://man.openbsd.org/sshd_config)
- [Проверка ключей сервера OpenSSH](https://man.openbsd.org/ssh#VERIFYING_HOST_KEYS)
- [Настройка UFW в Ubuntu](https://documentation.ubuntu.com/server/how-to/security/firewalls/index.html)
- [Обновление пакетов Ubuntu через APT](https://ubuntu.com/server/docs/tutorial/managing-software/)
- [Установка Nginx в Ubuntu](https://ubuntu.com/server/docs/how-to-install-nginx/)

## Личные кабинеты пользователей

После обновления CLEX MQTT кабинет доступен по `https://mqtt.example.com/account`. Регистрация включается явно настройкой `CLEX_REGISTRATION_ENABLED=true` в закрытом EnvironmentFile; `CLEX_MAX_ACCOUNTS` задаёт общий предел. До включения проверьте зарезервированное пространство топиков и квоты по [руководству аккаунтов](accounts.md). Миграции и бинарник обновляются обычной процедурой; резервная копия PostgreSQL включает данные кабинетов.

Главная `https://mqtt.example.com/` содержит вход в личный кабинет `/account` и ссылку `/account/register`, если регистрация включена. Вход администратора доступен напрямую по `/admin`. Общий proxy-маршрут Nginx должен обслуживать эти адреса вместе с главной страницей.

### Читаемые имена MQTT в аккаунтах

При обновлении установки с UUID-префиксами выполните [переход MQTT-имён аккаунтов](accounts.md#переход-с-прежних-mqtt-имён-аккаунтов) при остановленном брокере после проверки резервной копии. Затем перенесите новые MQTT-логины и префиксы из кабинета в настройки устройств.
