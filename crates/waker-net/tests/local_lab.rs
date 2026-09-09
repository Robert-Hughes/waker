use std::{
    net::{Ipv4Addr, SocketAddrV4},
    str::FromStr,
};

use waker_core::{MacAddress, WakeState, WakeTarget, run_wake};
use waker_net::{WakerWireGuardBackend, WireGuardProfile};

#[tokio::test]
#[ignore = "requires the local FreeBSD WireGuard lab"]
async fn wakes_through_real_wireguard_peer() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("waker_net=trace,gotatun=trace")
        .with_test_writer()
        .try_init();
    let profile_path = std::env::var("WAKER_LAB_PROFILE")
        .expect("set WAKER_LAB_PROFILE to the generated lab WireGuard profile");
    let profile_text = std::fs::read_to_string(profile_path).expect("read lab WireGuard profile");
    let profile = WireGuardProfile::parse(&profile_text).expect("parse lab WireGuard profile");

    let fritz_ip = Ipv4Addr::new(10, 231, 0, 1);
    let target = WakeTarget::new(
        MacAddress::from_str("AA:BB:CC:DD:EE:FF").expect("test MAC"),
        SocketAddrV4::new(fritz_ip, 2222),
    );
    let mut backend = WakerWireGuardBackend::new(profile, fritz_ip);
    let mut states = Vec::new();

    run_wake(&mut backend, &target, |state| {
        eprintln!("Waker state: {}", state.label());
        states.push(state);
    })
    .await
    .expect("wake through local WireGuard lab");

    assert!(matches!(states.first(), Some(WakeState::Connecting)));
    assert!(
        states
            .iter()
            .any(|state| matches!(state, WakeState::Waking))
    );
    assert!(
        states
            .iter()
            .any(|state| matches!(state, WakeState::WaitingForPc { .. }))
    );
    assert!(matches!(states.last(), Some(WakeState::Awake)));
}
