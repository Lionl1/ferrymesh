# Transports

A transport implements `ferrymesh_net::Transport`: it reports `PeerUp { peer, mtu }`,
`PeerDown`, `Frame`, and sends best-effort datagrams. Routing, encryption, deduplication,
retransmission (through `Hello`) and fragmentation are in the core, so a transport is
mostly discovery plus a datagram pipe. One `Host` runs any number of transports at once.

| Transport | State | MTU | Discovery |
|---|---|---|---|
| UDP (LAN / Wi-Fi / hotspot) | **works** | 1200 | broadcast beacons, `--peer` |
| Bluetooth LE | interface + plan | ATT MTU − 3 (20–512) | advertising / scanning |
| LoRa (Meshtastic board) | interface + plan | ~200 | the board's node list |
| Internet relay | interface + plan | 64 KiB | relay server |
| Wi-Fi Direct / Multipeer | interface + plan | 1200 (UDP on the group) | platform APIs |

## UDP — done

`crates/net/src/udp.rs`. Beacons `FMB1 ‖ instance` every 2 s to the broadcast address,
static peers and known neighbours; data `FMD1 ‖ frame`; a neighbour is down after 10 s of
silence. Works on any IP network, including a phone's hotspot.

Next: IPv6 link-local multicast (`ff02::…`) instead of IPv4 broadcast; mDNS service
advertisement for networks that drop broadcast.

## Bluetooth LE — next

1. A GATT service (one UUID) with a *write without response* characteristic (in) and a
   *notify* characteristic (out). Every node is both central and peripheral.
2. Desktop: [`btleplug`](https://crates.io/crates/btleplug) for the central role; the
   peripheral role needs platform code (BlueZ D-Bus on Linux, CoreBluetooth on macOS).
3. Phones: the core goes to Android/iOS through UniFFI; BLE stays in Kotlin/Swift and
   calls `peer_up` / `receive` across the FFI.
4. MTU = negotiated ATT MTU − 3; connection churn is normal, the core already tolerates it.

## LoRa — next

1. Talk to a Meshtastic-compatible board over USB serial (later BLE) with the Meshtastic
   protobuf API (`ToRadio` / `FromRadio`).
2. Carry ferrymesh frames as `PRIVATE_APP` packets, so the board's own chat keeps working;
   one Meshtastic node number = one `peer`.
3. LoRa is slow and duty-cycle limited: MTU ~200, `max_hello_ids` ≈ 4–8, longer
   `hello_interval`, Spray-and-Wait rather than epidemic, an airtime budget per hour.
4. Later: raw SX126x/SX127x radios without Meshtastic firmware.

## Internet relay — next

A small server that stores envelopes per destination address and pushes them over
WebSocket to subscribers. To the core it is one more neighbour, so nodes with mobile data
bridge the offline mesh to the internet. The relay sees only what any relay node sees.
Several relays can be configured; any node may run one.

## Wi-Fi Direct / Multipeer — later

Platform APIs only (Android Wi-Fi P2P / Wi-Fi Aware, iOS Multipeer Connectivity). Once a
group is formed it is an IP network, and the UDP transport runs on it unchanged.
