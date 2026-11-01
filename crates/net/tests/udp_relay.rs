//! Three real nodes on UDP over loopback, in a line: Alice and Carol know only Bob, so
//! everything between them goes through Bob, who cannot read it.

use std::net::SocketAddr;
use std::time::Duration;

use ferrymesh_core::{Config, Event, Identity, Node, OsEntropy};
use ferrymesh_net::{Host, HostHandle, UdpConfig, UdpTransport};
use tokio::time::timeout;

async fn udp() -> UdpTransport {
    UdpTransport::bind(UdpConfig {
        bind: SocketAddr::from(([127, 0, 0, 1], 0)),
        broadcast: None,
        beacon_interval: Duration::from_millis(100),
        peer_timeout: Duration::from_secs(2),
        ..UdpConfig::default()
    })
    .await
    .unwrap()
}

fn host(transport: UdpTransport) -> HostHandle {
    let config = Config {
        hello_interval_ms: 500,
        ..Config::default()
    };
    let mut host = Host::new(Node::new(Identity::generate(&mut OsEntropy), config));
    host.add_transport(transport);
    host.spawn()
}

#[tokio::test]
async fn message_crosses_a_relay_over_udp() {
    let mut a = udp().await;
    let b = udp().await;
    let mut c = udp().await;
    let b_addr = b.local_addr().unwrap();
    a.add_static_peer(b_addr);
    c.add_static_peer(b_addr);

    let mut alice = host(a);
    let mut bob = host(b);
    let mut carol = host(c);

    let id = alice
        .send(carol.public, b"hi Carol, via Bob".to_vec())
        .await
        .unwrap();

    let event = timeout(Duration::from_secs(10), carol.next_event())
        .await
        .expect("Carol got a message in time")
        .unwrap();
    let Event::Message {
        id: got,
        from,
        body,
        ..
    } = event
    else {
        panic!("expected a message, got {event:?}");
    };
    assert_eq!(
        (got, from, body.as_slice()),
        (id, alice.public, &b"hi Carol, via Bob"[..])
    );

    let receipt = timeout(Duration::from_secs(10), alice.next_event())
        .await
        .expect("Alice got the receipt in time")
        .unwrap();
    assert_eq!(receipt, Event::Delivered { id });

    // And back: Carol answers through the same relay.
    let reply = carol.send(alice.public, b"got it".to_vec()).await.unwrap();
    let event = timeout(Duration::from_secs(10), alice.next_event())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(event, Event::Message { id, .. } if id == reply));

    // Bob relayed both messages but received none himself.
    assert!(bob.try_next_event().is_none());
    let bob_node = bob.shutdown().await;
    assert!(!bob_node.carries(&id));
    alice.shutdown().await;
    carol.shutdown().await;
}
