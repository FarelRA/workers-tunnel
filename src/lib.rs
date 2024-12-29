use crate::proxy::{parse_early_data, parse_user_id, run_tunnel};
use crate::websocket::WebSocketStream;
use worker::*;

#[event(fetch)]
async fn main(req: Request, env: Env, _: Context) -> Result<Response> {
    // Add timeout to prevent infinite hanging
    let timeout = std::time::Duration::from_secs(50); // Cloudflare's limit is 60s
    let response = tokio::time::timeout(timeout, handle_request(req, env)).await;

    match response {
        Ok(result) => result,
        Err(_) => Response::error("Worker timed out", 504),
    }
}

async fn handle_request(req: Request, env: Env) -> Result<Response> {
    // get user id
    let user_id = env.var("USER_ID")?.to_string();
    let user_id = parse_user_id(&user_id);

    // get proxy ip list
    let proxy_ip = env.var("PROXY_IP")?.to_string();
    let proxy_ip = proxy_ip
        .split_ascii_whitespace()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect::<Vec<String>>();

    // better disguising
    let fallback_site = match env.var("FALLBACK_SITE") {
        Ok(f) => f.to_string(),
        Err(_) => String::from(""),
    };
    
    // Check for websocket upgrade
    let is_websocket = match req.headers().get("Upgrade")? {
        Some(up) => up.eq_ignore_ascii_case("websocket"),
        None => false,
    };

    if !is_websocket && !fallback_site.is_empty() {
        let req = Fetch::Url(Url::parse(&fallback_site)?);
        return req.send().await;
    }

    // ready early data
    let early_data = req.headers().get("sec-websocket-protocol")?;
    let early_data = parse_early_data(early_data)?;

    // Accept websocket connection with proper error handling
    let pair = WebSocketPair::new().map_err(|e| {
        console_error!("Failed to create WebSocket pair: {}", e);
        Error::from(e)
    })?;
    
    let WebSocketPair { client, server } = pair;
    
    server.accept().map_err(|e| {
        console_error!("Failed to accept WebSocket connection: {}", e);
        Error::from(e)
    })?;

    // Use a separate task for connection handling
    let handle = wasm_bindgen_futures::spawn_local(async move {
        let result = handle_connection(&server, early_data, user_id, proxy_ip).await;
        if let Err(err) = result {
            console_error!("Connection error: {}", err);
            // Ensure connection is properly closed on error
            let _ = server.close(Some(1011), Some(&format!("Error: {}", err)));
        }
    });

    // Return client socket immediately
    Response::from_websocket(client)
}

async fn handle_connection(
    server: &WebSocket,
    early_data: Option<Vec<u8>>,
    user_id: Vec<u8>,
    proxy_ip: Vec<String>,
) -> Result<()> {
    // Create websocket stream with proper error handling
    let socket = match server.events() {
        Ok(events) => WebSocketStream::new(server, events, early_data),
        Err(e) => {
            console_error!("Failed to get WebSocket events: {}", e);
            return Err(Error::from(e));
        }
    };

    // Run tunnel with proper cleanup
    let result = run_tunnel(socket, user_id, proxy_ip).await;
    
    // Ensure proper cleanup on error
    if let Err(ref e) = result {
        console_error!("Tunnel error: {}", e);
        let _ = server.close(Some(1011), Some(&format!("Tunnel error: {}", e)));
    }
    
    result
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
}

mod proxy {
    use std::io::{Error, ErrorKind, Result};
    use std::net::{Ipv4Addr, Ipv6Addr};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use crate::ext::StreamExt;
    use crate::protocol;
    use crate::websocket::WebSocketStream;
    use base64::{decode_config, URL_SAFE_NO_PAD};
    use tokio::io::{copy_bidirectional, AsyncReadExt, AsyncWriteExt};
    use worker::*;

    pub fn parse_early_data(data: Option<String>) -> Result<Option<Vec<u8>>> {
        if let Some(data) = data {
            if !data.is_empty() {
                let s = data.replace('+', "-").replace('/', "_").replace("=", "");
                match decode_config(s, URL_SAFE_NO_PAD) {
                    Ok(early_data) => return Ok(Some(early_data)),
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

        let mut bytes = Vec::new();
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
        // Add connection state tracking
        let is_connected = Arc::new(AtomicBool::new(true));
        let is_connected_clone = is_connected.clone();

        // Cleanup handler
        let _cleanup = defer::defer(move || {
            is_connected_clone.store(false, Ordering::SeqCst);
        });

        // read version
        if client_socket.read_u8().await? != protocol::VERSION {
            return Err(Error::new(ErrorKind::InvalidData, "invalid version"));
        }

        // verify user_id
        if client_socket.read_bytes(16).await? != user_id {
            return Err(Error::new(ErrorKind::InvalidData, "invalid user id"));
        }

        // ignore addons
        let length = client_socket.read_u8().await?;
        _ = client_socket.read_bytes(length as usize).await?;

        // read network type
        let network_type = client_socket.read_u8().await?;

        // read remote port
        let remote_port = client_socket.read_u16().await?;

        // read remote address
        let remote_addr = match client_socket.read_u8().await? {
            protocol::ADDRESS_TYPE_DOMAIN => {
                let length = client_socket.read_u8().await?;
                client_socket.read_string(length as usize).await?
            }
            protocol::ADDRESS_TYPE_IPV4 => {
                Ipv4Addr::from_bits(client_socket.read_u32().await?).to_string()
            }
            protocol::ADDRESS_TYPE_IPV6 => format!(
                "[{}]",
                Ipv6Addr::from_bits(client_socket.read_u128().await?)
            ),
            _ => {
                return Err(Error::new(ErrorKind::InvalidData, "invalid address type"));
            }
        };

        // process outbound with timeout and state tracking
        match network_type {
            protocol::NETWORK_TYPE_TCP => {
                process_tcp_outbound(&mut client_socket, &remote_addr, remote_port, is_connected).await
            }
            protocol::NETWORK_TYPE_UDP => {
                process_udp_outbound(&mut client_socket, &remote_addr, remote_port, is_connected).await
            }
            unknown => Err(Error::new(
                ErrorKind::InvalidData,
                format!("unsupported network type: {}", unknown),
            )),
        }
    }

    async fn process_tcp_outbound(
        client_socket: &mut WebSocketStream<'_>,
        target: &str,
        port: u16,
        is_connected: Arc<AtomicBool>,
    ) -> Result<()> {
        // connect to remote socket
        let mut remote_socket = Socket::builder().connect(target, port).map_err(|e| {
            Error::new(
                ErrorKind::ConnectionAborted,
                format!("connect to remote failed: {}", e),
            )
        })?;

        // check remote socket
        remote_socket.opened().await.map_err(|e| {
            Error::new(
                ErrorKind::ConnectionReset,
                format!("remote socket not opened: {}", e),
            )
        })?;

        // send response header
        client_socket
            .write(&protocol::RESPONSE)
            .await
            .map_err(|e| {
                Error::new(
                    ErrorKind::ConnectionAborted,
                    format!("send response header failed: {}", e),
                )
            })?;

        // Forward data with proper cleanup
        let result = copy_bidirectional(client_socket, &mut remote_socket).await;
        
        // Ensure connection is marked as closed
        is_connected.store(false, Ordering::SeqCst);
        
        result.map_err(|e| {
            Error::new(
                ErrorKind::ConnectionAborted,
                format!("forward data between client and remote failed: {}", e),
            )
        })?;

        Ok(())
    }

    async fn process_udp_outbound(
        client_socket: &mut WebSocketStream<'_>,
        target: &str,
        port: u16,
        is_connected: Arc<AtomicBool>,
    ) -> Result<()> {
        // check port (only support dns query)
        if port != 53 {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "not supported udp proxy yet",
            ));
        }

        // send response header
        client_socket
            .write(&protocol::RESPONSE)
            .await
            .map_err(|e| {
                Error::new(
                    ErrorKind::ConnectionAborted,
                    format!("send response header failed: {}", e),
                )
            })?;

        // forward data
        loop {
            // read packet length
            let length = client_socket.read_u16().await;
            if length.is_err() {
                return Ok(());
            }

            // read dns packet
            let packet = client_socket.read_bytes(length.unwrap() as usize).await?;

            // create request
            let request = Request::new_with_init("https://1.1.1.1/dns-query", &{
                // create request
                let mut init = RequestInit::new();
                init.method = Method::Post;
                init.headers = Headers::new();
                init.body = Some(packet.into());

                // set headers
                _ = init.headers.set("Content-Type", "application/dns-message");

                init
            })
            .unwrap();

            // invoke dns-over-http resolver
            let mut response = Fetch::Request(request).send().await.map_err(|e| {
                Error::new(
                    ErrorKind::ConnectionAborted,
                    format!("send DNS-over-HTTP request failed: {}", e),
                )
            })?;

            // read response
            let data = response.bytes().await.map_err(|e| {
                Error::new(
                    ErrorKind::ConnectionAborted,
                    format!("DNS-over-HTTP response body error: {}", e),
                )
            })?;

            // write response
            client_socket.write_u16(data.len() as u16).await?;
            client_socket.write_all(&data).await?;
		}
        
        // Mark connection as closed when done
        is_connected.store(false, Ordering::SeqCst);
        
        Ok(())
    }
}

mod websocket {
    use futures_util::Stream;
    use std::{
        io::{Error, ErrorKind, Result},
        pin::Pin,
        task::{Context, Poll},
    };

    use bytes::{BufMut, BytesMut};
    use pin_project::pin_project;
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
    use worker::{EventStream, WebSocket, WebsocketEvent};

    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[pin_project]
    pub struct WebSocketStream<'a> {
        ws: &'a WebSocket,
        #[pin]
        stream: EventStream<'a>,
        buffer: BytesMut,
        is_closed: Arc<AtomicBool>,
    }

    impl<'a> WebSocketStream<'a> {
        pub fn new(
            ws: &'a WebSocket,
            stream: EventStream<'a>,
            early_data: Option<Vec<u8>>,
        ) -> Self {
            let mut buffer = BytesMut::with_capacity(8192); // Preallocate buffer
            if let Some(data) = early_data {
                buffer.put_slice(&data)
            }

            Self {
                ws,
                stream,
                buffer,
                is_closed: Arc::new(AtomicBool::new(false)),
            }
        }
    }

    impl AsyncRead for WebSocketStream<'_> {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<Result<()>> {
            let mut this = self.project();

            // Check if connection is closed
            if this.is_closed.load(Ordering::SeqCst) {
                return Poll::Ready(Ok(()));
            }

            loop {
                let amt = std::cmp::min(this.buffer.len(), buf.remaining());
                if amt > 0 {
                    buf.put_slice(&this.buffer.split_to(amt));
                    return Poll::Ready(Ok(()));
                }

                match this.stream.as_mut().poll_next(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Some(Ok(WebsocketEvent::Message(msg)))) => {
                        if let Some(data) = msg.bytes() {
                            this.buffer.put_slice(&data);
                        };
                        continue;
                    }
                    Poll::Ready(Some(Ok(WebsocketEvent::Close(_)))) => {
                        this.is_closed.store(true, Ordering::SeqCst);
                        return Poll::Ready(Ok(()));
                    }
                    Poll::Ready(Some(Err(e))) => {
                        this.is_closed.store(true, Ordering::SeqCst);
                        return Poll::Ready(Err(Error::new(ErrorKind::Other, e.to_string())));
                    }
                    Poll::Ready(None) => {
                        this.is_closed.store(true, Ordering::SeqCst);
                        return Poll::Ready(Ok(()));
                    }
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
            // Check if connection is closed
            if self.is_closed.load(Ordering::SeqCst) {
                return Poll::Ready(Err(Error::new(ErrorKind::BrokenPipe, "connection closed")));
            }

            match self.ws.send_with_bytes(buf) {
                Ok(_) => Poll::Ready(Ok(buf.len())),
                Err(e) => {
                    self.is_closed.store(true, Ordering::SeqCst);
                    Poll::Ready(Err(Error::new(ErrorKind::Other, e.to_string())))
                }
            }
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<()>> {
            if self.is_closed.load(Ordering::SeqCst) {
                return Poll::Ready(Err(Error::new(ErrorKind::BrokenPipe, "connection closed")));
            }
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<()>> {
            if !self.is_closed.load(Ordering::SeqCst) {
                if let Err(e) = self.ws.close(None, Some("normal close")) {
                    return Poll::Ready(Err(Error::new(ErrorKind::Other, e.to_string())));
                }
                self.is_closed.store(true, Ordering::SeqCst);
            }
            Poll::Ready(Ok(()))
        }
    }
}