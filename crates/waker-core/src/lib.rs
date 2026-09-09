use std::{fmt, net::SocketAddrV4, str::FromStr, time::Duration};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct MacAddress([u8; 6]);

impl MacAddress {
    #[must_use]
    pub const fn octets(self) -> [u8; 6] {
        self.0
    }
}

impl fmt::Display for MacAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
            self.0[0], self.0[1], self.0[2], self.0[3], self.0[4], self.0[5]
        )
    }
}

impl FromStr for MacAddress {
    type Err = ParseMacAddressError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let normalized = value.replace('-', ":");
        let parts: Vec<_> = normalized.split(':').collect();
        if parts.len() != 6 {
            return Err(ParseMacAddressError(value.to_owned()));
        }

        let mut octets = [0_u8; 6];
        for (index, part) in parts.into_iter().enumerate() {
            if part.len() != 2 {
                return Err(ParseMacAddressError(value.to_owned()));
            }
            octets[index] =
                u8::from_str_radix(part, 16).map_err(|_| ParseMacAddressError(value.to_owned()))?;
        }
        Ok(Self(octets))
    }
}

#[derive(Debug, Error)]
#[error("invalid MAC address: {0}")]
pub struct ParseMacAddressError(String);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WakeTarget {
    pub mac: MacAddress,
    pub probe_address: SocketAddrV4,
    pub probe_timeout: Duration,
    pub probe_interval: Duration,
}

impl WakeTarget {
    #[must_use]
    pub fn new(mac: MacAddress, probe_address: SocketAddrV4) -> Self {
        Self {
            mac,
            probe_address,
            probe_timeout: Duration::from_mins(1),
            probe_interval: Duration::from_secs(1),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WakeState {
    Idle,
    Connecting,
    Waking,
    WaitingForPc { attempt: u32 },
    Awake,
    Failed(String),
}

impl WakeState {
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Idle => "Ready".to_owned(),
            Self::Connecting => "Connecting…".to_owned(),
            Self::Waking => "Waking…".to_owned(),
            Self::WaitingForPc { attempt } => format!("Waiting for PC… ({attempt})"),
            Self::Awake => "PC awake".to_owned(),
            Self::Failed(message) => format!("Failed: {message}"),
        }
    }
}

#[derive(Debug, Error)]
#[error("{0}")]
pub struct WakeBackendError(pub String);

impl WakeBackendError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

#[async_trait]
pub trait WakeBackend: Send {
    async fn connect(&mut self) -> Result<(), WakeBackendError>;
    async fn send_wake(&mut self, mac: MacAddress) -> Result<(), WakeBackendError>;
    async fn probe(&mut self, address: SocketAddrV4) -> Result<bool, WakeBackendError>;
    async fn disconnect(&mut self);
}

/// Run one complete wake attempt and report each state transition.
///
/// The backend is always disconnected after a connected wake attempt completes or fails.
///
/// # Errors
///
/// Returns the first connection, wake, or probe error, or a timeout if the target never becomes reachable.
pub async fn run_wake<B, F>(
    backend: &mut B,
    target: &WakeTarget,
    mut report: F,
) -> Result<(), WakeBackendError>
where
    B: WakeBackend,
    F: FnMut(WakeState) + Send,
{
    report(WakeState::Connecting);

    let result = async {
        backend.connect().await?;

        report(WakeState::Waking);
        backend.send_wake(target.mac).await?;

        let started = tokio::time::Instant::now();
        let mut attempt = 0_u32;
        loop {
            attempt = attempt.saturating_add(1);
            report(WakeState::WaitingForPc { attempt });

            if backend.probe(target.probe_address).await? {
                report(WakeState::Awake);
                return Ok(());
            }

            if started.elapsed() >= target.probe_timeout {
                return Err(WakeBackendError::new(format!(
                    "PC did not become reachable within {} seconds",
                    target.probe_timeout.as_secs()
                )));
            }

            tokio::time::sleep(target.probe_interval).await;
        }
    }
    .await;

    backend.disconnect().await;

    if let Err(error) = &result {
        report(WakeState::Failed(error.to_string()));
    }

    result
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, net::Ipv4Addr};

    use super::*;

    struct Backend {
        probes: VecDeque<bool>,
        disconnected: bool,
    }

    #[async_trait]
    impl WakeBackend for Backend {
        async fn connect(&mut self) -> Result<(), WakeBackendError> {
            Ok(())
        }

        async fn send_wake(&mut self, _mac: MacAddress) -> Result<(), WakeBackendError> {
            Ok(())
        }

        async fn probe(&mut self, _address: SocketAddrV4) -> Result<bool, WakeBackendError> {
            Ok(self.probes.pop_front().unwrap_or(false))
        }

        async fn disconnect(&mut self) {
            self.disconnected = true;
        }
    }

    #[test]
    fn parses_common_mac_formats() {
        let colon: MacAddress = "AA:BB:CC:DD:EE:FF".parse().unwrap();
        let dash: MacAddress = "aa-bb-cc-dd-ee-ff".parse().unwrap();
        assert_eq!(colon, dash);
        assert_eq!(colon.to_string(), "AA:BB:CC:DD:EE:FF");
    }

    #[tokio::test]
    async fn wake_sequence_disconnects_after_success() {
        let mut backend = Backend {
            probes: VecDeque::from([false, true]),
            disconnected: false,
        };
        let mut target = WakeTarget::new(
            "AA:BB:CC:DD:EE:FF".parse().unwrap(),
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, 22),
        );
        target.probe_interval = Duration::ZERO;

        let mut states = Vec::new();
        run_wake(&mut backend, &target, |state| states.push(state))
            .await
            .unwrap();

        assert!(backend.disconnected);
        assert_eq!(states.first(), Some(&WakeState::Connecting));
        assert_eq!(states.last(), Some(&WakeState::Awake));
    }
}
