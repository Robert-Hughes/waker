use std::{
    net::{IpAddr, SocketAddr},
    str::FromStr as _,
};

use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpListener,
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
    let pc_ip = IpAddr::from_str(
        &std::env::var("WAKER_LAB_PC_IP").unwrap_or_else(|_| "10.231.0.1".to_owned()),
    )
    .expect("valid WAKER_LAB_PC_IP");

    info!(%fritz_bind, %pc_ip, "Waker lab ready");
    run_fake_fritz(fritz_bind, pc_ip).await
}

async fn run_fake_fritz(bind: SocketAddr, pc_ip: IpAddr) -> std::io::Result<()> {
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
            let body = if text.contains("X_AVM-DE_WakeOnLANByMACAddress")
                && text.contains("<NewMACAddress>")
            {
                info!(%peer, "fake FRITZ received Wake-on-LAN request");
                Some(
                    "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body><u:X_AVM-DE_WakeOnLANByMACAddressResponse xmlns:u=\"urn:dslforum-org:service:Hosts:1\"/></s:Body></s:Envelope>"
                        .to_owned(),
                )
            } else if text.contains("GetSpecificHostEntry") && text.contains("<NewMACAddress>") {
                info!(%peer, %pc_ip, "fake FRITZ received host lookup");
                Some(format!(
                    "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body><u:GetSpecificHostEntryResponse xmlns:u=\"urn:dslforum-org:service:Hosts:1\"><NewIPAddress>{pc_ip}</NewIPAddress><NewActive>1</NewActive></u:GetSpecificHostEntryResponse></s:Body></s:Envelope>"
                ))
            } else {
                None
            };

            if let Some(body) = body {
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

fn content_length(request: &[u8]) -> Option<usize> {
    let headers = String::from_utf8_lossy(request);
    headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("Content-Length")
            .then(|| value.trim().parse::<usize>().ok())
            .flatten()
    })
}
