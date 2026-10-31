# ratatosk-core

Общее сетевое ядро для CLI, сервера и Ratatosk: конфигурация, протоколы,
транспорты, маршрутизация, DNS, локальные inbound-протоколы и userspace netstack.
Desktop UI и внешние процессы находятся в соседнем пакете `ratatosk`.

```sh
cargo test -p ratatosk-core
cargo build -p ratatosk-core --lib --target aarch64-apple-ios
```

См. [архитектуру workspace](../../docs/architecture.md).
