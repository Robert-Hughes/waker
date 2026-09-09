use std::{net::SocketAddr, time::Duration};

use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpListener,
    sync::watch,
};
use tracing::{info, warn};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("waker_lab=debug")),
        )
        .init();

    let fritz_bind: SocketAddr = std::env::var("WAKER_LAB_FRITZ_BIND")
        .unwrap_or_else(|_| "0.0.0.0:49000".to_owned())
        .parse()
        .expect("valid WAKER_LAB_FRITZ_BIND");
    let pc_bind: SocketAddr = std::env::var("WAKER_LAB_PC_BIND")
        .unwrap_or_else(|_| "0.0.0.0:2222".to_owned())
        .parse()
        .expect("valid WAKER_LAB_PC_BIND");
    let wake_delay = Duration::from_millis(
        std::env::var("WAKER_LAB_WAKE_DELAY_MS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(2_000),
    );

    let (awake_tx, awake_rx) = watch::channel(false);
    let fritz = run_fake_fritz(fritz_bind, awake_tx, wake_delay);
    let pc = run_fake_pc(pc_bind, awake_rx);

    info!(%fritz_bind, %pc_bind, ?wake_delay, "Waker lab ready");
    tokio::try_join!(fritz, pc)?;
    Ok(())
}

async fn run_fake_fritz(
    bind: SocketAddr,
    awake_tx: watch::Sender<bool>,
    wake_delay: Duration,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(bind).await?;
    loop {
        let (mut stream, peer) = match listener.accept().await {
            Ok(connection) => connection,
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionAborted => {
                warn!(%error, "fake FRITZ accept aborted; continuing");
                continue;
            }
            Err(error) => return Err(error),
        };
        let awake_tx = awake_tx.clone();
        tokio::spawn(async move {
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                match stream.read(&mut buffer).await {
                    Ok(0) => break,
                    Ok(count) => {
                        request.extend_from_slice(&buffer[..count]);
                        if request.windows(4).any(|window| window == b"\r\n\r\n")
                            && let Some(content_length) = content_length(&request)
                        {
                            let header_end = request
                                .windows(4)
                                .position(|window| window == b"\r\n\r\n")
                                .map_or(request.len(), |position| position + 4);
                            if request.len() >= header_end + content_length {
                                break;
                            }
                        }
                    }
                    Err(error) => {
                        warn!(%peer, %error, "fake FRITZ read failed");
                        return;
                    }
                }
            }

            let text = String::from_utf8_lossy(&request);
            let is_wol =
                text.contains("X_AVM-DE_WakeOnLANByMACAddress") && text.contains("<NewMACAddress>");
            if is_wol {
                info!(%peer, "fake FRITZ received Wake-on-LAN request");
                tokio::spawn(async move {
                    tokio::time::sleep(wake_delay).await;
                    let _ = awake_tx.send(true);
                    info!("fake PC is now reachable");
                });
                let body = "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body><u:X_AVM-DE_WakeOnLANByMACAddressResponse xmlns:u=\"urn:dslforum-org:service:Hosts:1\"/></s:Body></s:Envelope>";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
            } else {
                warn!(%peer, "fake FRITZ received unsupported request");
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await;
            }
        });
    }
}

async fn run_fake_pc(bind: SocketAddr, mut awake: watch::Receiver<bool>) -> std::io::Result<()> {
    while !*awake.borrow() {
        if awake.changed().await.is_err() {
            return Ok(());
        }
    }

    let listener = TcpListener::bind(bind).await?;
    info!(%bind, "fake PC probe port listening");
    loop {
        let (mut stream, peer) = match listener.accept().await {
            Ok(connection) => connection,
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionAborted => {
                warn!(%error, "fake PC accept aborted; continuing");
                continue;
            }
            Err(error) => return Err(error),
        };
        info!(%peer, "fake PC probe connected");
        let _ = stream.shutdown().await;
    }
}

fn content_length(request: &[u8]) -> Option<usize> {
    let headers = String::from_utf8_lossy(request);
    headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("Content-Length")
            .then(|| value.trim().parse::<usize>().ok())
            .flatten()
    })
}
