# Стенд citylivecam: сайт + h3-транспорт на одном 443

Рабочая конфигурация зарубежного VPS: домен отдаётся браузерам по HTTP/2 и
HTTP/3, а клиентам `ratatosk` — по своему QUIC-транспорту на том же `443/udp`.
Одна машина, один сертификат, один адрес. Всё, что нужно для стенда, лежит в
этом каталоге; секреты (домен, аккаунты) — только на сервере.

```
caddy           80/tcp, 443/tcp, 443/udp   сертификат LE, сайт для браузеров
citycam         внутренний 8080            сам сайт (закрытый репозиторий)
ratatosk-server 443/udp                    h3-транспорт, он же HTTP/3 для прочих
```

Почему так: Caddy и `ratatosk-server` слушают один и тот же `443/udp`.
Пакет, который не похож на HTTP/3-клиента Caddy (наш QUIC с другим ALPN),
разбирает `ratatosk-server`; обычный HTTP/3 он же обслуживает из `citycam`
через `site_backend`. Сертификат оба берут из тома `caddy_data` — поэтому
`ratatosk-server` монтирует его в `/data`.

## Что откуда берётся

| Файл | В репозитории | Где живёт |
|---|---|---|
| `docker-compose.yml`, `Caddyfile` | да | только репозиторий, на сервере не правятся |
| `setup.sh`, `accounts.sh`, `hardening.sh` | да | репозиторий |
| `.env` (домен, тег образа) | нет, создаётся `setup.sh` | сервер, `chmod 600` |
| `accounts.tsv` (метка → uuid) | нет, создаётся `accounts.sh` | сервер, `chmod 600` |
| `server.toml` | нет, генерируется из `accounts.tsv` | сервер |

Секретов в git нет: `accounts.tsv` — единственный файл, который нельзя
восстановить из репозитория, его надо скопировать с работающего сервера
(`scp`) или завести аккаунты заново.

## Новый сервер с нуля

1. Поднять машину (1 vCPU, 1 ГБ RAM, Ubuntu 24.04/26.04) и направить A-запись
   домена на её IP. Порт 80 должен быть доступен снаружи: сертификат Caddy
   получает по HTTP-01.

2. База и Docker (официальный репозиторий, не `get.docker.com`):

   ```sh
   apt update && apt -y upgrade
   apt -y install ca-certificates curl git jq ufw fail2ban iperf3
   install -m 0755 -d /etc/apt/keyrings
   curl -fsSL https://download.docker.com/linux/ubuntu/gpg -o /etc/apt/keyrings/docker.asc
   chmod a+r /etc/apt/keyrings/docker.asc
   echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/ubuntu $(. /etc/os-release && echo "$VERSION_CODENAME") stable" \
     >/etc/apt/sources.list.d/docker.list
   apt update
   apt -y install docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin
   systemctl enable --now docker
   ```

3. Код и стенд:

   ```sh
   git clone https://github.com/maxeliseyev/ratatosk.git /opt/ratatosk
   cd /opt/ratatosk/deploy/citylivecam
   bash setup.sh example.com          # .env, accounts.tsv, server.toml, ufw
   docker compose up -d               # caddy + citycam + ratatosk-server
   docker compose logs -f caddy       # ждать "certificate obtained successfully"
   ```

   Имя docker-проекта — имя каталога (`citylivecam`), поэтому тома и имена
   контейнеров совпадают с прежним сервером, даже если каталог другой.

4. Аккаунт клиента (ссылка печатается сразу, применяется по SIGHUP — без
   обрыва текущих соединений):

   ```sh
   ./accounts.sh add phone            # h3://…@example.com:443?sni=…
   ./accounts.sh list
   ./accounts.sh rm phone
   ./accounts.sh apply                # только если правили server.toml руками
   ```

5. Защита хоста (идемпотентно, можно запускать повторно):

   ```sh
   sudo bash hardening.sh
   ```

   Ставит sysctl под QUIC, swap 1 ГБ, ротацию логов Docker, лимит на SSH и
   fail2ban с двумя джейлами. Подробности ниже.

6. Перенос аккаунтов со старого сервера, если он ещё жив:

   ```sh
   scp old:/opt/ratatosk/deploy/citylivecam/accounts.tsv .
   ./accounts.sh regen && docker compose kill -s HUP ratatosk-server
   ```

## Обновление

```sh
cd /opt/ratatosk && git pull
cd deploy/citylivecam
docker compose pull && docker compose up -d
```

После обновления образа и при продлении сертификата (раз в 60 дней) сервер не
надо перезапускать: Caddy перевыпускает сертификат сам, `ratatosk-server`
перечитывает файлы только при старте, поэтому `docker compose restart
ratatosk-server` после продления — единственное обязательное действие.

Тег образа фиксируется в `.env`:

```
RATATOSK_SERVER_IMAGE=ghcr.io/maxeliseyev/ratatosk-server:0.20.2
```

`latest` удобен, но незаметно меняет поведение стенда: для отладки лучше
точный тег, выпущенный из git-тега `v*`.

## Проверка стенда

```sh
curl -sI https://example.com/ | head -3          # 200, alt-svc h3=":443"
dig +short example.com                            # IP машины
docker compose logs --tail=20 ratatosk-server     # "listening on 0.0.0.0:443/udp"
```

Сквозная проверка — `ratatosk` с ссылкой из `accounts.sh
list` (SOCKS5 на 1080):

```sh
ratatosk -u 'h3://…@example.com:443?sni=example.com#stand' -l 127.0.0.1 -p 1080
curl -x socks5h://127.0.0.1:1080 https://ifconfig.me
```

## Защита от сканов и флуда

Слоями, от дешёвого к дорогому:

1. **`ratatosk-server`** (`crates/server/README.md`, раздел «Ограничения
   ресурсов»): `Retry` до рукопожатия, 1024 соединения суммарно, 1024
   незавершённых рукопожатия в очереди, 16 соединений с одного IP, 4 МиБ
   окно приёма на соединение, 64 потока, 256 UDP-сессий. Всё в `server.toml`,
   значения по умолчанию рассчитаны на 1 ГБ RAM.
2. **Docker** (`docker-compose.yml`): `mem_limit` 128 МБ на сервис — утёкший
   или зафлуженный процесс убивается сам, хост не уходит в OOM; ротация логов
   10 МБ × 3, иначе json-лог растёт до заполнения диска.
3. **Ядро** (`hardening/sysctl.conf`, модуль `nf_conntrack` ставится в `modules-load.d`: без него `nf_conntrack_max` на загрузке игнорируется): буферы сокетов под QUIC (8 МБ),
   `netdev_max_backlog` и `somaxconn` под всплески рукопожатий,
   `nf_conntrack_max` 32768 — при 7168 записях флуд с чужих адресов выбивает
   conntrack, и после этого дропается вообще всё, включая легитимных клиентов.
4. **ufw**: входящее закрыто по умолчанию, открыты 22 (с лимитом 6 новых
   соединений на 30 с), 80/tcp, 443/tcp, 443/udp.
5. **fail2ban**: `sshd` (4 попытки за 10 минут → час бана; вход по ключу,
   пароли выключены) и `caddy-scan` (40 клиентских ошибок 400/403/404/405 за
   10 минут с одного адреса → час бана). Бан для контейнеров ставится в
   хук `forward` — трафик к контейнерам идёт через DNAT, а штатное действие
   fail2ban банит в `input` и на published-портах не делает ничего.

Чего здесь нет и быть не может: защиты от объёмного L3/L4-флуда (гигабиты),
от подмены BGP и от блокировки адреса провайдером. Это уровень хостера
(anti-DDoS) и anycast; наш слой отвечает только за то, чтобы машина не упала
от того трафика, который до неё дошёл.

Проверить, что джейлы работают:

```sh
sudo fail2ban-client status sshd
sudo fail2ban-client status caddy-scan
sudo fail2ban-client set caddy-scan banip 203.0.113.7   # тестовый бан
sudo nft list table inet f2b-forward                     # адрес в сете и правило
sudo fail2ban-client set caddy-scan unbanip 203.0.113.7
```
