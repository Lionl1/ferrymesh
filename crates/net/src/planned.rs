//! Transports that are designed but not written yet. Each has its config and its place
//! in the [`Transport`] interface; `open` returns
//! [`TransportError::NotYetImplemented`] with the plan. See `docs/transports.md`.

use tokio::sync::mpsc;

use crate::{Outgoing, Transport, TransportError, TransportEvent};

macro_rules! planned {
    ($(#[$doc:meta])* $name:ident, $config:ident, $kind:literal, $plan:literal) => {
        $(#[$doc])*
        pub struct $name {
            pub config: $config,
        }

        impl $name {
            pub const PLAN: &'static str = $plan;

            pub fn open(_config: $config) -> Result<Self, TransportError> {
                Err(TransportError::NotYetImplemented {
                    transport: $kind,
                    plan: Self::PLAN,
                })
            }
        }

        impl Transport for $name {
            fn kind(&self) -> &'static str {
                $kind
            }

            fn start(
                self: Box<Self>,
                _events: mpsc::Sender<TransportEvent>,
            ) -> mpsc::Sender<Outgoing> {
                unreachable!("{} cannot be opened yet", $kind)
            }
        }
    };
}

/// Bluetooth Low Energy settings.
#[derive(Debug, Clone, Default)]
pub struct BleConfig {
    /// Advertise and accept connections, not only scan.
    pub peripheral: bool,
}

planned!(
    /// Bluetooth Low Energy: a GATT service with one write and one notify characteristic,
    /// both roles (central and peripheral) on every node.
    BleTransport,
    BleConfig,
    "ble",
    "GATT service with write/notify characteristics; btleplug on desktops, \
     CoreBluetooth and Android BLE on phones; MTU = negotiated ATT MTU - 3"
);

/// A LoRa radio, e.g. a Meshtastic-compatible board over USB serial.
#[derive(Debug, Clone, Default)]
pub struct LoraConfig {
    /// `/dev/ttyUSB0`, `COM3`.
    pub serial_port: String,
}

planned!(
    /// LoRa through a Meshtastic-compatible board: ferrymesh frames ride in the
    /// firmware's private application port, so the board's own mesh stays usable.
    LoraTransport,
    LoraConfig,
    "lora",
    "Meshtastic serial API (protobuf ToRadio/FromRadio), frames in PRIVATE_APP packets, \
     MTU ~200 bytes, airtime budget and small Hello lists for the duty cycle"
);

/// An untrusted internet relay.
#[derive(Debug, Clone, Default)]
pub struct RelayConfig {
    /// `wss://relay.example.org`.
    pub url: String,
}

planned!(
    /// An internet relay used whenever mobile data or Wi-Fi with internet is available:
    /// a small server that stores and forwards envelopes by destination address. It sees
    /// only what any relay node sees: destination, size, time.
    RelayTransport,
    RelayConfig,
    "relay",
    "WebSocket to a small store-and-forward server; nodes subscribe to their own address \
     and act as one neighbour of each other through the server"
);

/// Wi-Fi Direct / Wi-Fi Aware between phones with no access point.
#[derive(Debug, Clone, Default)]
pub struct WifiDirectConfig;

planned!(
    /// Wi-Fi Direct (Android) and Multipeer Connectivity (iOS): phone-to-phone Wi-Fi
    /// without an access point; once a group is formed it carries the UDP transport.
    WifiDirectTransport,
    WifiDirectConfig,
    "wifi-direct",
    "platform APIs only (Android Wi-Fi P2P/Aware, iOS Multipeer); after group formation \
     the existing UDP transport runs over the group network"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planned_transports_explain_themselves() {
        let err = LoraTransport::open(LoraConfig::default()).err().unwrap();
        assert!(err.to_string().contains("lora"));
        assert!(BleTransport::open(BleConfig::default()).is_err());
        assert!(RelayTransport::open(RelayConfig::default()).is_err());
        assert!(WifiDirectTransport::open(WifiDirectConfig).is_err());
    }
}
