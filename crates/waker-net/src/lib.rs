use std::{
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4},
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use gotatun::{
    device::{DeviceBuilder, Peer},
    packet::{Ip, Packet, PacketBufPool},
    tun::{IpRecv, IpSend, MtuWatcher},
    udp::{UdpRecv, UdpSend, UdpTransportFactory, UdpTransportFactoryParams},
    x25519::{PublicKey, StaticSecret},
};
use ipnetwork::{IpNetwork, Ipv4Network};
use smoltcp::{
    iface::{Config as InterfaceConfig, Interface, SocketSet},
    phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken},
    socket::tcp,
    time::Instant as SmolInstant,
    wire::{HardwareAddress, IpAddress, IpCidr, Ipv4Address},
};
use thiserror::Error;
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
};
use tracing::{debug, trace};
use waker_core::{MacAddress, WakeBackend, WakeBackendError};

const TUNNEL_MTU: u16 = 1420;
const TCP_RX_BUFFER: usize = 32 * 1024;
const TCP_TX_BUFFER: usize = 16 * 1024;

#[derive(Clone)]
pub struct WireGuardProfile {
    pub address: Ipv4Network,
    private_key: [u8; 32],
    peer_public_key: [u8; 32],
    preshared_key: Option<[u8; 32]>,
    pub endpoint: String,
    pub allowed_ips: Vec<IpNetwork>,
    pub persistent_keepalive: Option<u16>,
}

impl WireGuardProfile {
    #[must_use]
    pub const fn has_preshared_key(&self) -> bool {
        self.preshared_key.is_some()
    }
    /// Parse the subset of a WireGuard/wg-quick profile needed by Waker.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed required fields, invalid keys/networks, or multiple peers.
    pub fn parse(input: &str) -> Result<Self, NetError> {
        #[derive(Clone, Copy, Eq, PartialEq)]
        enum Section {
            None,
            Interface,
            Peer,
        }

        let mut section = Section::None;
        let mut address = None;
        let mut private_key = None;
        let mut peer_public_key = None;
        let mut preshared_key = None;
        let mut endpoint = None;
        let mut allowed_ips = Vec::new();
        let mut persistent_keepalive = None;

        for (line_number, raw_line) in input.lines().enumerate() {
            let line = raw_line.split('#').next().unwrap_or_default().trim();
            if line.is_empty() {
                continue;
            }

            match line {
                "[Interface]" => {
                    section = Section::Interface;
                    continue;
                }
                "[Peer]" => {
                    if peer_public_key.is_some() {
                        return Err(NetError::Config(
                            "Waker currently supports exactly one WireGuard peer".to_owned(),
                        ));
                    }
                    section = Section::Peer;
                    continue;
                }
                _ => {}
            }

            let (key, value) = line.split_once('=').ok_or_else(|| {
                NetError::Config(format!("invalid WireGuard line {}", line_number + 1))
            })?;
            let key = key.trim();
            let value = value.trim();

            match (section, key) {
                (Section::Interface, "Address") => {
                    for candidate in value.split(',').map(str::trim) {
                        if let Ok(IpNetwork::V4(network)) = candidate.parse::<IpNetwork>() {
                            address = Some(network);
                            break;
                        }
                    }
                }
                (Section::Interface, "PrivateKey") => private_key = Some(parse_key(value)?),
                (Section::Peer, "PublicKey") => peer_public_key = Some(parse_key(value)?),
                (Section::Peer, "PresharedKey") => preshared_key = Some(parse_key(value)?),
                (Section::Peer, "Endpoint") => endpoint = Some(value.to_owned()),
                (Section::Peer, "AllowedIPs") => {
                    for network in value.split(',').map(str::trim) {
                        allowed_ips.push(network.parse::<IpNetwork>().map_err(|error| {
                            NetError::Config(format!("invalid AllowedIPs entry {network}: {error}"))
                        })?);
                    }
                }
                (Section::Peer, "PersistentKeepalive") => {
                    persistent_keepalive = Some(value.parse::<u16>().map_err(|error| {
                        NetError::Config(format!("invalid PersistentKeepalive: {error}"))
                    })?);
                }
                _ => {
                    // FRITZ!Box profiles can contain DNS and other wg-quick-only fields.
                    // Waker deliberately ignores fields it does not need.
                }
            }
        }

        let address = address.ok_or_else(|| {
            NetError::Config("WireGuard profile has no IPv4 Interface Address".to_owned())
        })?;
        let private_key = private_key
            .ok_or_else(|| NetError::Config("WireGuard profile has no PrivateKey".to_owned()))?;
        let peer_public_key = peer_public_key.ok_or_else(|| {
            NetError::Config("WireGuard profile has no peer PublicKey".to_owned())
        })?;
        let endpoint = endpoint
            .ok_or_else(|| NetError::Config("WireGuard profile has no peer Endpoint".to_owned()))?;
        if allowed_ips.is_empty() {
            return Err(NetError::Config(
                "WireGuard profile has no peer AllowedIPs".to_owned(),
            ));
        }

        Ok(Self {
            address,
            private_key,
            peer_public_key,
            preshared_key,
            endpoint,
            allowed_ips,
            persistent_keepalive,
        })
    }
}

fn parse_key(value: &str) -> Result<[u8; 32], NetError> {
    let bytes = BASE64
        .decode(value)
        .map_err(|error| NetError::Config(format!("invalid WireGuard key: {error}")))?;
    bytes
        .try_into()
        .map_err(|_| NetError::Config("WireGuard keys must decode to exactly 32 bytes".to_owned()))
}

#[derive(Debug, Error)]
pub enum NetError {
    #[error("configuration error: {0}")]
    Config(String),
    #[error("WireGuard error: {0}")]
    WireGuard(String),
    #[error("network error: {0}")]
    Network(String),
    #[error("operation timed out")]
    Timeout,
    #[error("tunnel worker stopped")]
    WorkerStopped,
}

// GotaTun normally requests a dual-stack outer UDP socket. FreeBSD rejects the
// resulting IPv4-mapped send path with EAFNOSUPPORT, so Waker binds the same
// address family as the resolved WireGuard peer instead.
#[derive(Clone, Copy)]
struct WakerUdpFactory {
    endpoint_ip: IpAddr,
}

impl WakerUdpFactory {
    const fn new(endpoint: SocketAddr) -> Self {
        Self {
            endpoint_ip: endpoint.ip(),
        }
    }
}

impl UdpTransportFactory for WakerUdpFactory {
    type Send = WakerUdpSocket;
    type Recv = WakerUdpSocket;

    async fn bind(
        &mut self,
        params: &UdpTransportFactoryParams,
    ) -> io::Result<(Self::Send, Self::Recv)> {
        let bind_ip = params.addr.unwrap_or(match self.endpoint_ip {
            IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
        });
        let socket = tokio::net::UdpSocket::bind(SocketAddr::new(bind_ip, params.port)).await?;
        let socket = WakerUdpSocket {
            inner: Arc::new(socket),
        };
        debug!(address = %socket.inner.local_addr()?, "bound Waker WireGuard UDP socket");
        Ok((socket.clone(), socket))
    }
}

#[derive(Clone)]
struct WakerUdpSocket {
    inner: Arc<tokio::net::UdpSocket>,
}

impl UdpSend for WakerUdpSocket {
    type SendManyBuf = ();

    async fn send_to(&self, packet: Packet, destination: SocketAddr) -> io::Result<()> {
        self.inner.send_to(&packet, destination).await?;
        Ok(())
    }

    fn local_addr(&self) -> io::Result<Option<SocketAddr>> {
        self.inner.local_addr().map(Some)
    }
}

impl UdpRecv for WakerUdpSocket {
    type RecvManyBuf = ();

    async fn recv_from(&mut self, pool: &mut PacketBufPool) -> io::Result<(Packet, SocketAddr)> {
        let mut packet = pool.get();
        let (length, source) = self.inner.recv_from(&mut packet).await?;
        packet.truncate(length);
        Ok((packet, source))
    }
}

struct GotaIpSend {
    incoming: mpsc::UnboundedSender<Vec<u8>>,
}

impl IpSend for GotaIpSend {
    async fn send(&mut self, packet: Packet<Ip>) -> io::Result<()> {
        let raw: Packet<[u8]> = packet.into();
        trace!(
            len = raw.as_ref().len(),
            "GotaTun delivered decrypted IP packet to smoltcp"
        );
        self.incoming
            .send(raw.as_ref().to_vec())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "smoltcp channel closed"))
    }
}

struct GotaIpRecv {
    outgoing: mpsc::UnboundedReceiver<Vec<u8>>,
}

impl IpRecv for GotaIpRecv {
    async fn recv<'a>(
        &'a mut self,
        _pool: &mut PacketBufPool,
    ) -> io::Result<impl Iterator<Item = Packet<Ip>> + Send + 'a> {
        let bytes = self.outgoing.recv().await.ok_or_else(|| {
            io::Error::new(io::ErrorKind::UnexpectedEof, "smoltcp channel closed")
        })?;
        trace!(
            len = bytes.len(),
            "GotaTun consumed outbound IP packet from smoltcp"
        );
        let raw: Packet<[u8]> = Packet::copy_from(bytes.as_slice());
        let parsed = raw.try_into_ipvx().map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid IP packet: {error}"),
            )
        })?;
        let packet = parsed.either(Packet::<Ip>::from, Packet::<Ip>::from);
        Ok(std::iter::once(packet))
    }

    fn mtu(&self) -> MtuWatcher {
        MtuWatcher::new(TUNNEL_MTU)
    }
}

struct PacketDevice {
    incoming: mpsc::UnboundedReceiver<Vec<u8>>,
    outgoing: mpsc::UnboundedSender<Vec<u8>>,
}

struct SmolRxToken {
    buffer: Vec<u8>,
}

impl RxToken for SmolRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.buffer)
    }
}

struct SmolTxToken {
    outgoing: mpsc::UnboundedSender<Vec<u8>>,
}

impl TxToken for SmolTxToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buffer = vec![0_u8; len];
        let result = f(&mut buffer);
        trace!(len, "smoltcp emitted outbound IP packet");
        let _ = self.outgoing.send(buffer);
        result
    }
}

impl Device for PacketDevice {
    type RxToken<'a> = SmolRxToken;
    type TxToken<'a> = SmolTxToken;

    fn capabilities(&self) -> DeviceCapabilities {
        let mut capabilities = DeviceCapabilities::default();
        capabilities.max_transmission_unit = usize::from(TUNNEL_MTU);
        capabilities.medium = Medium::Ip;
        capabilities
    }

    fn receive(
        &mut self,
        _timestamp: SmolInstant,
    ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        self.incoming.try_recv().ok().map(|buffer| {
            (
                SmolRxToken { buffer },
                SmolTxToken {
                    outgoing: self.outgoing.clone(),
                },
            )
        })
    }

    fn transmit(&mut self, _timestamp: SmolInstant) -> Option<Self::TxToken<'_>> {
        Some(SmolTxToken {
            outgoing: self.outgoing.clone(),
        })
    }
}

enum NetCommand {
    Http {
        target: SocketAddrV4,
        request: Vec<u8>,
        timeout: Duration,
        response: oneshot::Sender<Result<Vec<u8>, NetError>>,
    },
    Probe {
        target: SocketAddrV4,
        timeout: Duration,
        response: oneshot::Sender<Result<bool, NetError>>,
    },
}

struct SmolEngine {
    iface: Interface,
    device: PacketDevice,
    sockets: SocketSet<'static>,
    next_port: u16,
}

impl SmolEngine {
    fn new(
        address: Ipv4Network,
        incoming: mpsc::UnboundedReceiver<Vec<u8>>,
        outgoing: mpsc::UnboundedSender<Vec<u8>>,
    ) -> Self {
        let mut device = PacketDevice { incoming, outgoing };
        let mut config = InterfaceConfig::new(HardwareAddress::Ip);
        config.random_seed = rand::random();
        let mut iface = Interface::new(config, &mut device, SmolInstant::now());
        let ip = to_smol_ipv4(address.ip());
        iface.update_ip_addrs(|addresses| {
            addresses
                .push(IpCidr::new(IpAddress::Ipv4(ip), address.prefix()))
                .expect("one address fits in the interface address list");
        });

        // A WireGuard/TUN-style IP medium is point-to-point; the gateway is only
        // used by smoltcp's route selection and does not result in ARP/ND traffic.
        iface
            .routes_mut()
            .add_default_ipv4_route(Ipv4Address::new(0, 0, 0, 1))
            .expect("one default route fits in the route table");

        Self {
            iface,
            device,
            sockets: SocketSet::new(Vec::new()),
            next_port: 49_152,
        }
    }

    async fn run(mut self, mut commands: mpsc::Receiver<NetCommand>) {
        while let Some(command) = commands.recv().await {
            match command {
                NetCommand::Http {
                    target,
                    request,
                    timeout,
                    response,
                } => {
                    let result = self.http(target, &request, timeout).await;
                    let _ = response.send(result);
                }
                NetCommand::Probe {
                    target,
                    timeout,
                    response,
                } => {
                    let result = self.probe(target, timeout).await;
                    let _ = response.send(result);
                }
            }
        }
    }

    async fn http(
        &mut self,
        target: SocketAddrV4,
        request: &[u8],
        timeout: Duration,
    ) -> Result<Vec<u8>, NetError> {
        let handle = self.open_socket(target)?;
        let deadline = tokio::time::Instant::now() + timeout;
        let mut sent = false;
        let mut response = Vec::new();

        loop {
            self.iface
                .poll(SmolInstant::now(), &mut self.device, &mut self.sockets);

            let mut complete = false;
            let mut failed = false;
            {
                let socket = self.sockets.get_mut::<tcp::Socket>(handle);

                if socket.state() == tcp::State::Closed && !sent {
                    failed = true;
                }

                if !sent && socket.may_send() {
                    let count = socket
                        .send_slice(request)
                        .map_err(|error| NetError::Network(format!("TCP send failed: {error}")))?;
                    if count != request.len() {
                        return Err(NetError::Network(
                            "HTTP request did not fit in the smoltcp transmit buffer".to_owned(),
                        ));
                    }
                    sent = true;
                    socket.close();
                }

                while socket.can_recv() {
                    socket
                        .recv(|bytes| {
                            response.extend_from_slice(bytes);
                            (bytes.len(), ())
                        })
                        .map_err(|error| {
                            NetError::Network(format!("TCP receive failed: {error}"))
                        })?;
                }

                if sent && !socket.may_recv() {
                    complete = true;
                }
            }

            if failed {
                self.sockets.remove(handle);
                return Err(NetError::Network(format!(
                    "TCP connection to {target} failed"
                )));
            }
            if complete {
                self.sockets.remove(handle);
                return Ok(response);
            }
            if tokio::time::Instant::now() >= deadline {
                self.sockets.remove(handle);
                return Err(NetError::Timeout);
            }

            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    async fn probe(&mut self, target: SocketAddrV4, timeout: Duration) -> Result<bool, NetError> {
        let handle = self.open_socket(target)?;
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            self.iface
                .poll(SmolInstant::now(), &mut self.device, &mut self.sockets);

            let state = self.sockets.get::<tcp::Socket>(handle).state();
            if state == tcp::State::Established {
                self.sockets.get_mut::<tcp::Socket>(handle).abort();
                self.iface
                    .poll(SmolInstant::now(), &mut self.device, &mut self.sockets);
                self.sockets.remove(handle);
                return Ok(true);
            }
            if state == tcp::State::Closed {
                self.sockets.remove(handle);
                return Ok(false);
            }
            if tokio::time::Instant::now() >= deadline {
                self.sockets.remove(handle);
                return Ok(false);
            }

            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn open_socket(
        &mut self,
        target: SocketAddrV4,
    ) -> Result<smoltcp::iface::SocketHandle, NetError> {
        let rx = tcp::SocketBuffer::new(vec![0_u8; TCP_RX_BUFFER]);
        let tx = tcp::SocketBuffer::new(vec![0_u8; TCP_TX_BUFFER]);
        let socket = tcp::Socket::new(rx, tx);
        let handle = self.sockets.add(socket);

        let local_port = self.next_port;
        self.next_port = if self.next_port == u16::MAX {
            49_152
        } else {
            self.next_port + 1
        };

        let remote = (IpAddress::Ipv4(to_smol_ipv4(*target.ip())), target.port());
        if let Err(error) = self.sockets.get_mut::<tcp::Socket>(handle).connect(
            self.iface.context(),
            remote,
            local_port,
        ) {
            self.sockets.remove(handle);
            return Err(NetError::Network(format!(
                "TCP connect setup failed: {error}"
            )));
        }
        Ok(handle)
    }
}

fn to_smol_ipv4(address: Ipv4Addr) -> Ipv4Address {
    let [a, b, c, d] = address.octets();
    Ipv4Address::new(a, b, c, d)
}

pub struct TunnelClient {
    commands: mpsc::Sender<NetCommand>,
    shutdown: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
}

impl TunnelClient {
    /// Create an isolated userspace `WireGuard` device and its private smoltcp interface.
    ///
    /// # Errors
    ///
    /// Returns an error if the peer endpoint cannot be resolved or `GotaTun` cannot create the device.
    pub async fn connect(profile: WireGuardProfile) -> Result<Self, NetError> {
        let endpoint = resolve_endpoint(&profile.endpoint).await?;
        debug!(%endpoint, "starting in-process WireGuard peer");

        let (wg_to_smol_tx, wg_to_smol_rx) = mpsc::unbounded_channel();
        let (smol_to_wg_tx, smol_to_wg_rx) = mpsc::unbounded_channel();
        let ip_send = GotaIpSend {
            incoming: wg_to_smol_tx,
        };
        let ip_recv = GotaIpRecv {
            outgoing: smol_to_wg_rx,
        };

        let mut peer = Peer::new(PublicKey::from(profile.peer_public_key))
            .with_endpoint(endpoint)
            .with_allowed_ips(profile.allowed_ips.clone());
        peer.keepalive = profile.persistent_keepalive;
        if let Some(preshared_key) = profile.preshared_key {
            peer = peer.with_preshared_key(preshared_key);
        }

        let device = DeviceBuilder::new()
            .with_udp(WakerUdpFactory::new(endpoint))
            .with_ip_pair(ip_send, ip_recv)
            .with_private_key(StaticSecret::from(profile.private_key))
            .with_peer(peer)
            .build()
            .await
            .map_err(|error| NetError::WireGuard(error.to_string()))?;

        let (shutdown, mut shutdown_rx) = watch::channel(false);
        let device_task = tokio::spawn(async move {
            let _device = device;
            loop {
                if *shutdown_rx.borrow() {
                    break;
                }
                if shutdown_rx.changed().await.is_err() {
                    break;
                }
            }
            debug!("WireGuard device stopped");
        });

        let (commands, command_rx) = mpsc::channel(8);
        let engine = SmolEngine::new(profile.address, wg_to_smol_rx, smol_to_wg_tx);
        let engine_task = tokio::spawn(engine.run(command_rx));

        Ok(Self {
            commands,
            shutdown,
            tasks: vec![device_task, engine_task],
        })
    }

    /// Send one complete HTTP request through the private WireGuard/smoltcp stack.
    ///
    /// # Errors
    ///
    /// Returns an error if TCP setup, transmission, reception, or the tunnel worker fails or times out.
    pub async fn http(
        &self,
        target: SocketAddrV4,
        request: Vec<u8>,
        timeout: Duration,
    ) -> Result<Vec<u8>, NetError> {
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(NetCommand::Http {
                target,
                request,
                timeout,
                response: tx,
            })
            .await
            .map_err(|_| NetError::WorkerStopped)?;
        rx.await.map_err(|_| NetError::WorkerStopped)?
    }

    /// Attempt a TCP connection through the private WireGuard/smoltcp stack.
    ///
    /// # Errors
    ///
    /// Returns an error if the tunnel worker has stopped or the private stack cannot perform the probe.
    pub async fn probe(&self, target: SocketAddrV4, timeout: Duration) -> Result<bool, NetError> {
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(NetCommand::Probe {
                target,
                timeout,
                response: tx,
            })
            .await
            .map_err(|_| NetError::WorkerStopped)?;
        rx.await.map_err(|_| NetError::WorkerStopped)?
    }

    pub async fn shutdown(mut self) {
        let _ = self.shutdown.send(true);
        drop(self.commands);
        for task in self.tasks.drain(..) {
            task.abort();
            let _ = task.await;
        }
    }
}

async fn resolve_endpoint(endpoint: &str) -> Result<SocketAddr, NetError> {
    if let Ok(address) = SocketAddr::from_str(endpoint) {
        return Ok(address);
    }

    let mut resolved = tokio::net::lookup_host(endpoint)
        .await
        .map_err(|error| NetError::Network(format!("could not resolve {endpoint}: {error}")))?;
    resolved
        .next()
        .ok_or_else(|| NetError::Network(format!("endpoint {endpoint} resolved to no addresses")))
}

const FRITZ_HOSTS_SERVICE: &str = "urn:dslforum-org:service:Hosts:1";

fn fritz_hosts_request(fritz_ip: Ipv4Addr, action: &str, arguments: &str) -> Vec<u8> {
    let body = format!(
        "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body><u:{action} xmlns:u=\"{FRITZ_HOSTS_SERVICE}\">{arguments}</u:{action}></s:Body></s:Envelope>"
    );
    format!(
        "POST /upnp/control/hosts HTTP/1.1\r\nHost: {fritz_ip}:49000\r\nContent-Type: text/xml; charset=\"utf-8\"\r\nSOAPAction: \"{FRITZ_HOSTS_SERVICE}#{action}\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
    .into_bytes()
}

#[must_use]
pub fn fritz_wol_request(fritz_ip: Ipv4Addr, mac: MacAddress) -> Vec<u8> {
    fritz_hosts_request(
        fritz_ip,
        "X_AVM-DE_WakeOnLANByMACAddress",
        &format!("<NewMACAddress>{mac}</NewMACAddress>"),
    )
}

fn http_status(response: &[u8]) -> Result<u16, NetError> {
    let first_line = response
        .split(|byte| *byte == b'\n')
        .next()
        .ok_or_else(|| NetError::Network("empty HTTP response".to_owned()))?;
    let line = std::str::from_utf8(first_line)
        .map_err(|_| NetError::Network("HTTP status line was not UTF-8".to_owned()))?;
    let mut parts = line.trim_end_matches('\r').split_whitespace();
    let _version = parts.next();
    parts
        .next()
        .ok_or_else(|| NetError::Network("HTTP response had no status code".to_owned()))?
        .parse::<u16>()
        .map_err(|error| NetError::Network(format!("invalid HTTP status: {error}")))
}

fn fritz_host_active(response: &[u8]) -> Result<bool, NetError> {
    let text = std::str::from_utf8(response)
        .map_err(|_| NetError::Network("FRITZ!Box host response was not UTF-8".to_owned()))?;
    let start_tag = "<NewActive>";
    let end_tag = "</NewActive>";
    let start = text
        .find(start_tag)
        .map(|index| index + start_tag.len())
        .ok_or_else(|| {
            NetError::Network("FRITZ!Box host response had no NewActive field".to_owned())
        })?;
    let value = text[start..]
        .split_once(end_tag)
        .map(|(value, _)| value.trim())
        .ok_or_else(|| {
            NetError::Network(
                "FRITZ!Box host response had an incomplete NewActive field".to_owned(),
            )
        })?;
    match value {
        "1" | "true" => Ok(true),
        "0" | "false" => Ok(false),
        _ => Err(NetError::Network(format!(
            "FRITZ!Box returned invalid NewActive value {value:?}"
        ))),
    }
}

pub struct WakerWireGuardBackend {
    profile: WireGuardProfile,
    fritz_address: SocketAddrV4,
    tunnel: Option<TunnelClient>,
}

impl WakerWireGuardBackend {
    #[must_use]
    pub fn new(profile: WireGuardProfile, fritz_ip: Ipv4Addr) -> Self {
        Self {
            profile,
            fritz_address: SocketAddrV4::new(fritz_ip, 49_000),
            tunnel: None,
        }
    }

    fn tunnel(&self) -> Result<&TunnelClient, WakeBackendError> {
        self.tunnel
            .as_ref()
            .ok_or_else(|| WakeBackendError::new("WireGuard tunnel is not connected"))
    }

    async fn hosts_action(
        &self,
        action: &str,
        arguments: &str,
    ) -> Result<(u16, Vec<u8>), WakeBackendError> {
        let request = fritz_hosts_request(*self.fritz_address.ip(), action, arguments);
        let response = self
            .tunnel()?
            .http(self.fritz_address, request, Duration::from_secs(8))
            .await
            .map_err(to_backend_error)?;
        let status = http_status(&response).map_err(to_backend_error)?;
        if !(200..300).contains(&status) {
            return Err(WakeBackendError::new(format!(
                "FRITZ!Box Hosts action {action} returned HTTP {status}"
            )));
        }
        Ok((status, response))
    }

    /// Ask the FRITZ!Box whether the target host is currently active.
    ///
    /// The `WireGuard` tunnel must already be connected.
    ///
    /// # Errors
    ///
    /// Returns an error if the Hosts service request fails or `NewActive` is missing or invalid.
    pub async fn host_active(&self, mac: MacAddress) -> Result<bool, WakeBackendError> {
        let arguments = format!("<NewMACAddress>{mac}</NewMACAddress>");
        let (_status, response) = self
            .hosts_action("GetSpecificHostEntry", &arguments)
            .await?;
        let active = fritz_host_active(&response).map_err(to_backend_error)?;
        debug!(%mac, active, "FRITZ!Box host status received");
        Ok(active)
    }
}

#[async_trait]
impl WakeBackend for WakerWireGuardBackend {
    async fn connect(&mut self) -> Result<(), WakeBackendError> {
        let tunnel = TunnelClient::connect(self.profile.clone())
            .await
            .map_err(to_backend_error)?;

        trace!(target = %self.fritz_address, "probing FRITZ!Box through WireGuard");
        let reachable = tunnel
            .probe(self.fritz_address, Duration::from_secs(8))
            .await
            .map_err(to_backend_error)?;
        if !reachable {
            tunnel.shutdown().await;
            return Err(WakeBackendError::new(format!(
                "FRITZ!Box {} was not reachable through WireGuard",
                self.fritz_address
            )));
        }

        self.tunnel = Some(tunnel);
        debug!(target = %self.fritz_address, "FRITZ!Box reachable through WireGuard");
        Ok(())
    }

    async fn send_wake(&mut self, mac: MacAddress) -> Result<(), WakeBackendError> {
        let arguments = format!("<NewMACAddress>{mac}</NewMACAddress>");
        let (status, _response) = self
            .hosts_action("X_AVM-DE_WakeOnLANByMACAddress", &arguments)
            .await?;
        debug!(status, "FRITZ!Box accepted Wake-on-LAN request");
        Ok(())
    }

    async fn probe(&mut self, address: SocketAddrV4) -> Result<bool, WakeBackendError> {
        let reachable = self
            .tunnel()?
            .probe(address, Duration::from_secs(2))
            .await
            .map_err(to_backend_error)?;
        debug!(target = %address, reachable, "PC reachability probe completed");
        Ok(reachable)
    }

    async fn disconnect(&mut self) {
        if let Some(tunnel) = self.tunnel.take() {
            tunnel.shutdown().await;
            debug!("WireGuard tunnel disconnected");
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
fn to_backend_error(error: NetError) -> WakeBackendError {
    WakeBackendError::new(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_PROFILE: &str = r"
[Interface]
PrivateKey = AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=
Address = 10.231.0.2/24
DNS = 1.1.1.1

[Peer]
PublicKey = AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=
PresharedKey = AgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgI=
AllowedIPs = 10.231.0.0/24, 192.0.2.0/24
Endpoint = 127.0.0.1:51820
PersistentKeepalive = 25
";

    #[test]
    fn parses_fritz_style_wireguard_profile() {
        let profile = WireGuardProfile::parse(TEST_PROFILE).unwrap();
        assert_eq!(profile.address.ip(), Ipv4Addr::new(10, 231, 0, 2));
        assert_eq!(profile.address.prefix(), 24);
        assert_eq!(profile.endpoint, "127.0.0.1:51820");
        assert_eq!(profile.allowed_ips.len(), 2);
        assert_eq!(profile.persistent_keepalive, Some(25));
        assert!(profile.preshared_key.is_some());
    }

    #[test]
    fn wol_request_is_targeted_at_hosts_service() {
        let request = fritz_wol_request(
            Ipv4Addr::new(192, 168, 178, 1),
            "AA:BB:CC:DD:EE:FF".parse().unwrap(),
        );
        let text = String::from_utf8(request).unwrap();
        assert!(text.starts_with("POST /upnp/control/hosts HTTP/1.1\r\n"));
        assert!(text.contains("X_AVM-DE_WakeOnLANByMACAddress"));
        assert!(text.contains("AA:BB:CC:DD:EE:FF"));
    }

    #[test]
    fn parses_http_status() {
        assert_eq!(http_status(b"HTTP/1.1 200 OK\r\n\r\n").unwrap(), 200);
    }

    #[test]
    fn host_status_request_is_targeted_at_hosts_service() {
        let request = fritz_hosts_request(
            Ipv4Addr::new(192, 168, 178, 1),
            "GetSpecificHostEntry",
            "<NewMACAddress>AA:BB:CC:DD:EE:FF</NewMACAddress>",
        );
        let text = String::from_utf8(request).unwrap();
        assert!(text.starts_with("POST /upnp/control/hosts HTTP/1.1\r\n"));
        assert!(text.contains("GetSpecificHostEntry"));
        assert!(text.contains("AA:BB:CC:DD:EE:FF"));
    }

    #[test]
    fn parses_fritz_host_active_values() {
        let active = b"HTTP/1.1 200 OK\r\n\r\n<NewActive>1</NewActive>";
        let inactive = b"HTTP/1.1 200 OK\r\n\r\n<NewActive>0</NewActive>";
        assert!(fritz_host_active(active).unwrap());
        assert!(!fritz_host_active(inactive).unwrap());
    }

    #[test]
    fn rejects_missing_fritz_host_active_value() {
        let response = b"HTTP/1.1 200 OK\r\n\r\n<NewHostName>Example-PC</NewHostName>";
        assert!(fritz_host_active(response).is_err());
    }
}
