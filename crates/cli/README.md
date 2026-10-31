# ratatosk — CLI-клиент

Бинарник `ratatosk`, терминальный интерфейс, журнал, службы и системный
TUN-режим (`--tun`, нативный in-process netstack на macOS; xray-core
используется как sidecar только для XTLS Vision поверх plain TLS).
Использует независимый `ratatosk-core`.

```sh
cargo run -p ratatosk -- --help
cargo build -p ratatosk --release
```

См. [использование клиента](../../README.md) и
[архитектуру workspace](../../docs/architecture.md).
