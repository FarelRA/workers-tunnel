use crate::proxy::{parse_early_data, parse_user_id, run_tunnel};
use crate::websocket::WebSocketStream;
use std::time::Duration;
use worker::*;

const TUNNEL_TIMEOUT: Duration = Duration::from_secs(300); // 5 minutes timeout
const MAX_BUFFER_SIZE: usize = 32 * 1024; // 32KB buffer limit

#[event(fetch)]
async fn main(req: Request, env: Env, ctx: Context) -> Result<Response> {
    // Set a deadline for the entire worker
    ctx.wait_until(async {
        tokio::time::sleep(TUNNEL_TIMEOUT).await;
    });

    // Parse configuration
    let config = match parse_config(&env) {
        Ok(config) => config,
        Err(e) => return Response::error(format!("Configuration error: {}", e), 500),
    };

    // Handle fallback if needed
    if should_fallback(&req, &config.fallback_site)? {
        return handle_fallback(&config.fallback_site).await;
    }

    // Handle WebSocket connection
    handle_websocket(req, config).await
}

struct Config {
    user_id: Vec<u8>,
    proxy_ip: Vec<String>,
    fallback_site: String,
}

fn parse_config(env: &Env) -> Result<Config> {
    let user_id = env
        .var("USER_ID")
        .map_err(|e| format!("USER_ID not set: {}", e))?
        .to_string();
    let user_id = parse_user_id(&user_id);

    let proxy_ip = env
        .var("PROXY_IP")
        .map_err(|e| format!("PROXY_IP not set: {}", e))?
        .to_string();
    let proxy_ip = proxy_ip
        .split_ascii_whitespace()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();

    let fallback_site = env
        .var("FALLBACK_SITE")
        .map(|f| f.to_string())
        .unwrap_or_default();

    Ok(Config {
        user_id,
        proxy_ip,
        fallback_site,
    })
}

fn should_fallback(req: &Request, fallback_site: &str) -> Result<bool> {
    if fallback_site.is_empty() {
        return Ok(false);
    }
    
    match req.headers().get("Upgrade")? {
        Some(up) => Ok(up != *"websocket"),
        None => Ok(true),
    }
}

async fn handle_fallback(fallback_site: &str) -> Result<Response> {
    let req = Fetch::Url(Url::parse(fallback_site)?);
    req.send().await
}

async fn handle_websocket(req: Request, config: Config) -> Result<Response> {
    // Parse early data
    let early_data = match req.headers().get("sec-websocket-protocol")? {
        Some(protocol) => parse_early_data(Some(protocol))?,
        None => None,
    };

    // Create WebSocket pair
    let pair = WebSocketPair::new()?;
    let server = pair.server;
    let client = pair.client;

    // Accept connection
    server.accept()?;

    // Spawn tunnel handler
    wasm_bindgen_futures::spawn_local(async move {
        let result = handle_tunnel(&server, early_data, config).await;
        
        if let Err(err) = result {
            console_error!("Tunnel error: {}", err);
            let _ = server.close(Some(1011), Some(&err.to_string()));
        }
    });

    Response::from_websocket(client)
}

async fn handle_tunnel(
    server: &WebSocket,
    early_data: Option<Vec<u8>>,
    config: Config,
) -> Result<()> {
    let events = server.events().map_err(|e| format!("Failed to get server events: {}", e))?;
    let socket = WebSocketStream::new(server, events, early_data);

    match tokio::time::timeout(
        TUNNEL_TIMEOUT,
        run_tunnel(socket, config.user_id, config.proxy_ip),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(Error::new(
            std::io::ErrorKind::TimedOut,
            "Tunnel timed out",
        )),
    }
}

#[allow(dead_code)]
mod protocol {
    pub const VERSION: u8 = 0;
    pub const RESPONSE: [u8; 2] = [0u8; 2];
    pub const NETWORK_TYPE_TCP: u8 = 1;
    pub const NETWORK_TYPE_UDP: u8 = 2;
    pub const ADDRESS_TYPE_IPV4: u8 = 1;
    pub const ADDRESS_TYPE_DOMAIN: u8 = 2;
    pub const ADDRESS_TYPE_IPV6: u8 = 3;
    
    // Added new constants for error handling
    pub const ERROR_TIMEOUT: u16 = 1001;
    pub const ERROR_INVALID_DATA: u16 = 1002;
    pub const ERROR_CONNECTION_FAILED: u16 = 1003;
}

mod proxy {
    use std::io::{Error, ErrorKind, Result};
    use std::net::{Ipv4Addr, Ipv6Addr};
    use std::time::Duration;

    use crate::ext::StreamExt;
    use crate::protocol;
    use crate::websocket::WebSocketStream;
    use base64::{decode_config, URL_SAFE_NO_PAD};
    use tokio::io::{copy_bidirectional, AsyncReadExt, AsyncWriteExt};
    use tokio::time::timeout;
    use worker::*;

    const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
    const DNS_TIMEOUT: Duration = Duration::from_secs(5);

    pub fn parse_early_data(data: Option<String>) -> Result<Option<Vec<u8>>> {
        if let Some(data) = data {
            if !data.is_empty() {
                let s = data.replace('+', "-").replace('/', "_").replace("=", "");
                match decode_config(s, URL_SAFE_NO_PAD) {
                    Ok(early_data) => {
                        if early_data.len() > crate::MAX_BUFFER_SIZE {
                            return Err(Error::new(
                                ErrorKind::InvalidData,
                                "Early data exceeds maximum buffer size",
                            ));
                        }
                        return Ok(Some(early_data));
                    }
                    Err(err) => return Err(Error::new(ErrorKind::Other, err.to_string())),
                }
            }
        }
        Ok(None)
    }

    pub fn parse_user_id(user_id: &str) -> Vec<u8> {
        let mut hex_bytes = user_id
            .as_bytes()
            .iter()
            .filter_map(|b| match b {
                b'0'..=b'9' => Some(b - b'0'),
                b'a'..=b'f' => Some(b - b'a' + 10),
                b'A'..=b'F' => Some(b - b'A' + 10),
                _ => None,
            })
            .fuse();

        let mut bytes = Vec::with_capacity(16); // Pre-allocate for expected size
        while let (Some(h), Some(l)) = (hex_bytes.next(), hex_bytes.next()) {
            bytes.push((h << 4) | l)
        }
        bytes
    }

    pub async fn run_tunnel(
        mut client_socket: WebSocketStream<'_>,
        user_id: Vec<u8>,
        proxy_ip: Vec<String>,
    ) -> Result<()> {
        // Validate protocol version
        if client_socket.read_u8().await? != protocol::VERSION {
            return Err(Error::new(ErrorKind::InvalidData, "Invalid protocol version"));
        }

        // Verify user ID
        let received_user_id = client_socket.read_bytes(16).await?;
        if received_user_id != user_id {
            return Err(Error::new(ErrorKind::InvalidData, "Invalid user ID"));
        }

        // Read and validate addons length
        let length = client_socket.read_u8().await?;
        if length as usize > crate::MAX_BUFFER_SIZE {
            return Err(Error::new(ErrorKind::InvalidData, "Addons data too large"));
        }
        _ = client_socket.read_bytes(length as usize).await?;

        // Read network type
        let network_type = client_socket.read_u8().await?;

        // Read remote port
        let remote_port = client_socket.read_u16().await?;

        // Read and validate remote address
        let remote_addr = read_remote_address(&mut client_socket).await?;

        // Process outbound connection
        match network_type {
            protocol::NETWORK_TYPE_TCP => {
                process_tcp_outbound(&mut client_socket, proxy_ip, &remote_addr, remote_port).await
            }
            protocol::NETWORK_TYPE_UDP => {
                if remote_port != 53 {
                    return Err(Error::new(
                        ErrorKind::InvalidData,
                        "UDP proxy only supports DNS queries (port 53)",
                    ));
                }
                process_udp_outbound(&mut client_socket).await
            }
            _ => Err(Error::new(
                ErrorKind::InvalidData,
                format!("Unsupported network type: {}", network_type),
            )),
        }
    }

    async fn read_remote_address(client_socket: &mut WebSocketStream<'_>) -> Result<String> {
        match client_socket.read_u8().await? {
            protocol::ADDRESS_TYPE_DOMAIN => {
                let length = client_socket.read_u8().await?;
                if length as usize > 255 {
                    return Err(Error::new(ErrorKind::InvalidData, "Domain name too long"));
                }
                client_socket.read_string(length as usize).await?
            }
            protocol::ADDRESS_TYPE_IPV4 => {
                Ipv4Addr::from_bits(client_socket.read_u32().await?).to_string()
            }
            protocol::ADDRESS_TYPE_IPV6 => {
                format!("[{}]", Ipv6Addr::from_bits(client_socket.read_u128().await?))
            }
            addr_type => Err(Error::new(
                ErrorKind::InvalidData,
                format!("Invalid address type: {}", addr_type),
            ))?,
        }
    }

    async fn process_tcp_outbound(
        client_socket: &mut WebSocketStream<'_>,
        proxy_ip: Vec<String>,
        remote_addr: &str,
        port: u16,
    ) -> Result<()> {
        let mut last_error = None;

        // Try direct connection first, then proxy IPs
        for target in std::iter::once(remote_addr.to_string()).chain(proxy_ip) {
            match timeout(CONNECT_TIMEOUT, connect_tcp(&target, port)).await {
                Ok(Ok(mut remote_socket)) => {
                    // Send success response
                    client_socket.write_all(&protocol::RESPONSE).await?;

                    // Forward data with timeout
                    match timeout(
                        crate::TUNNEL_TIMEOUT,
                        copy_bidirectional(client_socket, &mut remote_socket),
                    )
                    .await
                    {
                        Ok(Ok(_)) => return Ok(()),
                        Ok(Err(e)) => {
                            if e.kind() != ErrorKind::ConnectionReset {
                                return Err(e);
                            }
                        }
                        Err(_) => {
                            return Err(Error::new(ErrorKind::TimedOut, "Connection timed out"));
                        }
                    }
                }
                Ok(Err(e)) => last_error = Some(e),
                Err(_) => last_error = Some(Error::new(ErrorKind::TimedOut, "Connection timed out")),
            }
        }

        Err(last_error.unwrap_or_else(|| {
            Error::new(ErrorKind::ConnectionRefused, "No available targets to connect")
        }))
    }

    async fn connect_tcp(target: &str, port: u16) -> Result<Socket> {
        let socket = Socket::builder().connect(target, port).map_err(|e| {
            Error::new(
                ErrorKind::ConnectionAborted,
                format!("Failed to connect to {}: {}", target, e),
            )
        })?;

        socket.opened().await.map_err(|e| {
            Error::new(
                ErrorKind::ConnectionReset,
                format!("Socket not opened for {}: {}", target, e),
            )
        })?;

        Ok(socket)
    }

    async fn process_udp_outbound(client_socket: &mut WebSocketStream<'_>) -> Result<()> {
        // Send success response
        client_socket.write_all(&protocol::RESPONSE).await?;

        loop {
            // Read packet length with timeout
            let length = match timeout(DNS_TIMEOUT, client_socket.read_u16()).await {
                Ok(Ok(len)) => len,
                Ok(Err(e)) => {
                    if e.kind() == ErrorKind::UnexpectedEof {
                        return Ok(());
                    }
                    return Err(e);
                }
                Err(_) => return Err(Error::new(ErrorKind::TimedOut, "DNS query timed out")),
            };

            if length as usize > crate::MAX_BUFFER_SIZE {
                return Err(Error::new(ErrorKind::InvalidData, "DNS packet too large"));
            }

            // Read DNS packet
            let packet = client_socket.read_bytes(length as usize).await?;

            // Create and send DNS-over-HTTPS request
            let response = timeout(
                DNS_TIMEOUT,
                send_dns_query(packet, client_socket.ws_version()),
            )
            .await
            .map_err(|_| Error::new(ErrorKind::TimedOut, "DNS-over-HTTPS request timed out"))??;

            // Write response
            client_socket.write_u16(response.len() as u16).await?;
            client_socket.write_all(&response).await?;
        }
    }

    async fn send_dns_query(packet: Vec<u8>, ws_version: &str) -> Result<Vec<u8>> {
        let mut headers = Headers::new();
        headers.set("Content-Type", "application/dns-message")?;
        headers.set("User-Agent", &format!("CF-Worker-DNS/{}", ws_version))?;

        let mut init = RequestInit::new();
        init.method = Method::Post;
        init.headers = headers;
        init.body = Some(packet.into());

        let request = Request::new_with_init(
            "https://1.1.1.1/dns-query",
            &init,
        )?;

        let response = Fetch::Request(request)
            .send()
            .await
            .map_err(|e| Error::new(ErrorKind::Other, format!("DNS-over-HTTPS request failed: {}", e)))?;

        response
            .bytes()
            .await
            .map_err(|e| Error::new(ErrorKind::Other, format!("Failed to read DNS response: {}", e)))
    }
}

mod websocket {
    use futures_util::Stream;
    use std::{
        io::{Error, ErrorKind, Result},
        pin::Pin,
        sync::atomic::{AtomicBool, Ordering},
        task::{Context, Poll},
    };

    use bytes::{BufMut, BytesMut};
    use pin_project::pin_project;
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
    use worker::{EventStream, WebSocket, WebsocketEvent};

    #[pin_project]
    pub struct WebSocketStream<'a> {
        ws: &'a WebSocket,
        #[pin]
        stream: EventStream<'a>,
        buffer: BytesMut,
        is_closed: AtomicBool,
    }

    impl<'a> WebSocketStream<'a> {
        pub fn new(
            ws: &'a WebSocket,
            stream: EventStream<'a>,
            early_data: Option<Vec<u8>>,
        ) -> Self {
            let mut buffer = BytesMut::with_capacity(crate::MAX_BUFFER_SIZE);
            if let Some(data) = early_data {
                buffer.put_slice(&data);
            }

            Self {
                ws,
                stream,
                buffer,
                is_closed: AtomicBool::new(false),
            }
        }
    }

    impl AsyncRead for WebSocketStream<'_> {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<Result<()>> {
            let this = self.project();

            if this.is_closed.load(Ordering::Relaxed) {
                return Poll::Ready(Ok(()));
            }

            if !this.buffer.is_empty() {
                let amt = std::cmp::min(this.buffer.len(), buf.remaining());
                buf.put_slice(&this.buffer.split_to(amt));
                return Poll::Ready(Ok(()));
            }

            match this.stream.as_mut().poll_next(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(Some(Ok(WebsocketEvent::Message(msg)))) => {
                    if let Some(data) = msg.bytes() {
                        if data.len() > crate::MAX_BUFFER_SIZE {
                            return Poll::Ready(Err(Error::new(
                                ErrorKind::InvalidData,
                                "Message exceeds maximum buffer size",
                            )));
                        }
                        this.buffer.put_slice(&data);
                        let amt = std::cmp::min(this.buffer.len(), buf.remaining());
                        buf.put_slice(&this.buffer.split_to(amt));
                    }
                    Poll::Ready(Ok(()))
                }
                Poll::Ready(Some(Ok(WebsocketEvent::Close(_)))) => {
                    this.is_closed.store(true, Ordering::Relaxed);
                    Poll::Ready(Ok(()))
                }
                Poll::Ready(Some(Err(e))) => {
                    Poll::Ready(Err(Error::new(ErrorKind::Other, e.to_string())))
                }
                Poll::Ready(None) => {
                    this.is_closed.store(true, Ordering::Relaxed);
                    Poll::Ready(Ok(()))
                }
            }
        }
    }

    impl AsyncWrite for WebSocketStream<'_> {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<Result<usize>> {
            if self.is_closed.load(Ordering::Relaxed) {
                return Poll::Ready(Err(Error::new(
                    ErrorKind::BrokenPipe,
                    "WebSocket connection is closed",
                )));
            }

            if buf.len() > crate::MAX_BUFFER_SIZE {
                return Poll::Ready(Err(Error::new(
                    ErrorKind::InvalidData,
                    "Write buffer exceeds maximum size",
                )));
            }

            match self.ws.send_with_bytes(buf) {
                Ok(_) => Poll::Ready(Ok(buf.len())),
                Err(e) => {
                    self.is_closed.store(true, Ordering::Relaxed);
                    Poll::Ready(Err(Error::new(ErrorKind::Other, e.to_string())))
                }
            }
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<()>> {
            if self.is_closed.load(Ordering::Relaxed) {
                return Poll::Ready(Err(Error::new(
                    ErrorKind::BrokenPipe,
                    "WebSocket connection is closed",
                )));
            }
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<()>> {
            if !self.is_closed.load(Ordering::Relaxed) {
                if let Err(e) = self.ws.close(None, Some("normal close")) {
                    return Poll::Ready(Err(Error::new(ErrorKind::Other, e.to_string())));
                }
                self.is_closed.store(true, Ordering::Relaxed);
            }
            Poll::Ready(Ok(()))
        }
    }
}