# iOS: RatatoskCore.xcframework

The core as a static library for the Packet Tunnel extension, plus the Swift
code that drives it.

## Take it from a release

Every tagged release carries:

| Asset | What |
|-------|------|
| `RatatoskCore.xcframework.zip` | device `arm64` + simulator `arm64` (static lib + `ratatosk.h` + modulemap, module name `RatatoskCore`) |
| `RatatoskCore.xcframework.zip.sha256` | checksum, for `binaryTarget(url:checksum:)` |
| `ratatosk-swift.zip` | `RatatoskCore.swift`, `RatatoskTools.swift`, `PacketTunnelProvider.swift` |

From GitHub:

```sh
gh release download v0.20.9 --repo maxeliseyev/ratatosk -p 'RatatoskCore.xcframework.zip*' -p 'ratatosk-swift.zip'
unzip RatatoskCore.xcframework.zip
```

## Wire it into the app

1. Drag `RatatoskCore.xcframework` into the *Packet Tunnel extension* target and
   set **Embed & Sign** (or keep it out of "Copy Bundle Resources" if the app
   links it statically — a static library needs neither).
2. Add the three Swift files from `ratatosk-swift.zip` to the same target.
   `RatatoskCore.swift` wraps the C ABI (`RatatoskEngine`), `PacketTunnelProvider.swift`
   is the `NEPacketTunnelProvider` template.
3. Capabilities: *Network Extensions* (packet tunnel), and the App Group the
   engine persists its links and logs to.

## Local build

```sh
scripts/build-xcframework.sh                 # target/RatatoskCore.xcframework
scripts/build-xcframework.sh /tmp/RatatoskCore.xcframework
```

It builds `ratatosk-ffi` for `aarch64-apple-ios` and `aarch64-apple-ios-sim`
with the `ios` profile (thin LTO), then lets `xcodebuild` assemble the
framework. `IPHONEOS_DEPLOYMENT_TARGET` defaults to 16.0 and must stay at or
below the deployment target of the C code that aws-lc compiles.

## What the core expects from the host

- Packets in and out through `ratatosk_push_packet` / the outbound callback; the
  extension reads `packetFlow` and writes what the core produces.
- DNS: the tunnel announces the netstack's FakeDNS address as the resolver
  (`RATATOSK_DNS_ADDRESS`), so names are resolved inside the tunnel.
- The core's own transport must stay outside the tunnel: the extension sets the
  tunnel's routes and excludes the exit-server address (iOS does this per-flow
  through the packet tunnel's `includedRoutes`), which is what
  `PacketTunnelProvider.swift` does.
