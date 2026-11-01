# План: энергоэффективность и бенчмаркинг Ratatosk

Цель: сделать клиент (desktop / Android / iOS) экономнее по CPU и батарее,
замерить эффект и не потерять его при дальнейших изменениях.

Контекст: разбор PORTAL_WG (форк WG Tunnel + AmneziaWG + mihomo/tgwss +
gVisor) показал, что Ratatosk архитектурно уже легче: один процесс, Rust,
свой netstack, REALITY вместо мусорных пакетов, Vision direct copy. Этот план
закрывает оставшиеся точечные гэпы и добавляет способ это доказывать цифрами.

## Правила процесса

- Один коммит = один пункт чеклиста. Не смешиваем правки и бенчмарк-инфраструктуру.
- Сначала baseline, потом правка, потом повторный замер. Без цифр правку не считаем готовой.
- Ядро (`crates/core`) общее: правка там достаётся desktop, Android и iOS разом.
- Регрессии ловим до мержа: `cargo test --workspace --locked`, `cargo clippy`,
  `cargo fmt --all --check`, сборка обоих мобильных клиентов.
- Все результаты измерений — в `results/`, в имени файла дата и платформа.

## Фаза 0 — Бенчмарк-харнесс (инфраструктура, делаем ДО правок)

Нужен единый способ измерять три вещи: **idle CPU**, **CPU под нагрузкой**,
**память (RSS)**. Плюс трафик/счётчики (уже есть через `STATS`).

### 0.1 Desktop-бенчмарк ядра (Rust)

- [x] Добавить `crates/core/examples/bench.rs` (или `scripts/bench-core.sh`):
  - [x] поднимает локальный mock VLESS-сервер (echo, переиспользован паттерн из `crates/core/tests/e2e.rs`);
  - [x] гоняет ядро с SOCKS-inbound: фаза idle + фаза загрузки (полнодуплексный трансфер);
  - [x] снимает CPU через `getrusage(RUSAGE_SELF)` и RSS (Linux: `/proc/self/statm`, macOS: `task_info(TASK_VM_INFO)`);
  - [x] печатает JSON: `{idle.cpu_pct, load.cpu_pct, cpu_per_mbit, rss_idle_mb, rss_load_mb, down_mbps, up_mbps}`.
- [ ] Прогнать на macOS и Linux, сохранить `results/bench-core-<дата>-<платформа>.json` (macOS уже в `results/bench-core-20261101-macos.json`, Linux — отложено до машины с Linux).

### 0.2 Android-бенчмарк (adb)

- [x] Добавить `scripts/android-bench.sh` (в `ratatosk-android`):
  - [x] `adb install` APK и импорт ссылки через intent (`--apk`, `--link`);
  - [x] idle: окно с выключенным экраном, CPU процесса из `/proc/<pid>/stat` (jiffies) + PSS из `dumpsys meminfo`;
  - [x] load: загрузка URL через туннель (`--load`), та же выборка;
  - [x] JSON-вывод. «Энергия» через `dumpsys batterystats` отложена: сначала CPU/RSS, потом решим.
- [x] Прогнать idle на Redmi Note 8 Pro (300 с, экран выкл): `idle_cpu_pct 0.62 %`, `pss 71 MB` → `results/bench-android-20261101-idle.json`.
- [ ] Load на Android: отложен — в shell нет curl/wget (`nc` через h3 не вернул данные на :80). Вариант: запушить статический curl-бинарник.

### 0.3 iOS-бенчмарк (расширить существующие watcher'ы)

- [x] В `RatatoskTunnel/TunnelLog.swift` рядом с `MemoryWatch` добавить
  `CpuWatch` (сэмплирует CPU% процесса через `getrusage` раз в 10 с,
  логирует `cpu: N.N% (user + sys)` в `TunnelLog`) и подключить в `PacketTunnelProvider`.
- [ ] Опционально `ThermalWatch` (`ProcessInfo.thermalState`) — ловить троттлинг.
- [ ] Батарея: долгий прогон через Instruments/MetricKit вручную, скрипт не обязателен.
- [ ] Прогнать на устройстве: логи `cpu:`/`memory:` из `TunnelLog` за idle-сессию (нужно iPhone + сборка Xcode).

### 0.4 Baseline-замеры (фиксируем текущее состояние)

- [x] Desktop: idle CPU, RSS, CPU/Mбит/с — `results/BASELINE.md` + `results/bench-core-20261101-macos.json`.
- [x] Android (Redmi Note 8 Pro): idle CPU% 0.62 %, PSS 71 МБ — `results/bench-android-20261101-idle.json`.
- [ ] iOS (если есть устройство): логи `cpu:` и `memory:` из `TunnelLog` за idle-сессию.

## Фаза 1 — TCP_NODELAY + keepalive (ядро, все платформы)

Сейчас `set_nodelay`/`set_keepalive` нигде не ставятся → Nagle добавляет задержку
на мелких пакетах, а мёртвые мобильные соединения висят до `idle_timeout_secs=600`.

- [x] В `crates/core/src/proxy/runtime.rs`:
  - [x] `direct_tcp`: после `socket.connect` → `tune_tcp(&stream)` (nodelay + keepalive).
  - [x] путь к серверу (`connect_remote`): `TcpStream::connect` → `tune_tcp` для Reality/None/WS.
  - [x] `transport/tls.rs::connect_tls`: `tune_tcp(&tcp)` (покрывает TLS-пути).
  - [x] `transport/xhttp.rs::dial`: `tune_tcp(&tcp)` для Reality/None.
- [x] keepalive через `libc::setsockopt`: `SO_KEEPALIVE` + `TCP_KEEPIDLE`(30)/`TCP_KEEPINTVL`(10)/`TCP_KEEPCNT`(3) (на macOS `TCP_KEEPALIVE`). `tune_tcp` — общий `pub(crate)` хелпер.
- [x] Тесты: `cargo test -p ratatosk-core` 163+10 ok, `clippy` без новых warning'ов, `fmt` ok.
- [x] Re-baseline desktop: без регрессии (idle 0.005 %, `cpu_per_mbit` в шуме) → `results/bench-core-20261101-after-tcp-tune.json`.
- [ ] Latency-валидация nodelay: loopback не показывает Nagle (нет delayed ACK). Замерить на реальном TCP-сервере (reality/tls/ws) или на устройстве — отдельным прогоном.

## Фаза 2 — TUN reader: epoll + eventfd (Android)

Сейчас `rust/src/lib.rs::spawn_reader` крутит `poll(fd, POLLIN, 200ms)` в цикле →
5 пробуждений CPU/сек даже в простое.

- [x] В `ratatosk-android/rust/src/lib.rs`:
  - [x] `eventfd` + `epoll` вместо `poll(200ms)`: reader блокируется в `epoll_wait` без таймаута;
  - [x] `nativeStop` → `signal_stop(eventfd)` → reader просыпается мгновенно;
  - [x] `drain_tun` читает пакеты до EAGAIN (level-triggered, без зацикливания);
  - [x] `READ_POLL_MS` переименован в `WRITE_POLL_MS` (остался только для POLLOUT в `write_all`).
- [x] Проверено на устройстве: APK собран (`versionCode 5`, in-place upgrade, данные сохранены), туннель поднимается, трафик идёт.
- [x] **Метрика пробуждений reader-потока: 7 voluntary ctx switches / 60 с** против ~300 у старого `poll(200ms)` (5/с) → минус ~98 %.
- [ ] Idle CPU% не изменился значимо (0.6–1.3 % — шум фонового трафика телефона; польза epoll в числе пробуждений/батарее, а не в process CPU%).
- [ ] Чистый disconnect/reconnect + отсутствие утечки fd — ручная проверка (UI был на заблокированном экране).

## Фаза 3 — network_changed для TCP/REALITY (ядро + оба клиента)

Сейчас `runtime.rs::network_changed` мигрирует только h3 (QUIC). TCP/REALITY/WS и
Mux-соединения на Wi-Fi↔LTE остаются полуоткрытыми до таймаутов.

- [x] В `crates/core/src/protocol/mux.rs`: добавить `MuxManager::invalidate()`
  (помечает все `Connection.retired = true` и закрывает idle), чтобы следующий
  `open` набрал новое соединение.
- [x] В `crates/core/src/proxy/runtime.rs::network_changed`:
  - [x] вызвать `self.mux.invalidate()` (если mux включён);
  - [x] оставить h3-миграцию как есть.
- [x] Android: `nativeNetworkChanged` → `connector.network_changed()` (уже wired, `ConnectivityManager`-колбэк).
- [x] iOS: `NetworkWatch` → `engine.networkChanged()` → FFI `ratatosk_network_changed` (уже wired).
- [x] Тесты: юнит `invalidate_dials_a_fresh_connection` (после invalidate новый `open` диалит свежее соединение).
- [ ] Проверка на живом устройстве: переключение Wi-Fi↔LTE (нужен реальный сменный интерфейс; на Wi-Fi не автоматизировать).

## Фаза 4 — Адаптивный поллинг статистики (оба клиента)

Оба клиента поллят `stats`/`flows` раз в 1 с, только когда UI на экране. Уже
неплохо, можно ещё мягче.

- [x] Android `TunnelController.poll()`: интервал 1 с при движении трафика, 3 с в тишине (по изменению `up`/`down`); сброс на первый счётчик. Собрано (`compileDebugKotlin` + `assembleRelease` v6).
- [x] iOS `TunnelController.pollStats()`: то же самое (сравнение `TrafficStats` по `Equatable`); конструкции проверены `swiftc -typecheck`.
- [ ] Проверить на устройстве, что лог потоков и скорость в UI не «замирают» визуально (устройство отключилось — APK v6 готов к установке).

## Фаза 5 — Финальные замеры и документирование

- [x] Прогнать полный бенчмарк: desktop (`bench-core-20261101-final.json`) + Android v6 (`idle_cpu_pct 0.68 %`, `pss 72 МБ`, reader wakeups 1/60с).
- [x] Пересобрать iOS XCFramework (`scripts/build-xcframework.sh`) и обновить iOS-репо (`update-core.sh`): оба таргета типчекаются против нового фреймворка.
- [x] Свести в `docs/energy-comparison.md`: baseline vs after по idle CPU, RSS, CPU/Mбит/с + читабельное сравнение с PORTAL_WG.
- [ ] Обновить README: короткий раздел «Энергоэффективность» с цифрами (в работе).
- [x] Опционально: сравнение с PORTAL_WG уже внутри `docs/energy-comparison.md` (отдельный публичный doc — по желанию позже).
- [ ] Android-load: всё ещё отложен (нужен статический curl-бинарь).
- [x] iOS: XCFramework пересобран, Swift-обёртки обновлены, typecheck обоих таргетов чист.
- [ ] iOS: прогон на живом iPhone (сборка Xcode + логи `cpu:`/`memory:`).
- [x] Роутер OpenWRT: обновлён на ratatosk 0.19.0 (h3 + REALITY), оба канала проверены end-to-end.

## Порядок и зависимости

```
0 (harness + baseline)
  → 1 (nodelay/keepalive)   [независимо, низкий риск]
  → 2 (epoll, Android)      [независимо, Android-only]
  → 3 (network_changed)     [после 1, затрагивает ядро]
  → 4 (adaptive polling)    [независимо, косметика]
  → 5 (итоговые замеры)
```

Фазы 1, 2, 4 можно делать параллельно/в любом порядке после baseline; фаза 3 —
после 1 (тот же участок кода). Фаза 5 — только после всех.

## Definition of Done

- Каждый пункт закрыт коммитом, прошедшим `cargo test/clippy/fmt` и сборку
  обоих мобильных клиентов.
- `results/` содержит baseline и after-замеры по каждой платформе.
- Android idle CPU с включённым туннелем ≈ 0 % (после фазы 2).
- iOS логируют `cpu:`/`memory:` в `TunnelLog` (после фазы 0.3).
