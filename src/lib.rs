use crate::proxy::{parse_early_data, parse_user_id, run_tunnel};
use crate::websocket::WebSocketStream;
use worker::*;

#[event(fetch)]
pub async fn main(req: Request, env: Env, _: Context) -> Result<Response> {
    // Quick check for WebSocket upgrade
    if !matches!(req.headers().get("Upgrade")?, Some(up) if up == "websocket") {
        if let Ok(site) = env.var("FALLBACK_SITE") {
            return Fetch::Url(Url::parse(&site.to_string())?).send().await;
        }
        return Response::error("Bad Request", 400);
    }

    let user_id = parse_user_id(&env.var("USER_ID")?.to_string());
    let proxy_ip = env.var("PROXY_IP")?
        .to_string()
        .split_ascii_whitespace()
        .map(String::from)
        .collect::<Vec<_>>();

    let early_data = parse_early_data(req.headers().get("sec-websocket-protocol")?)?;

    let pair = WebSocketPair::new()?;
    let server = pair.server;
    let client = pair.client;

    server.accept()?;

    wasm_bindgen_futures::spawn_local(async move {
        let socket = WebSocketStream::new(&server, early_data);
        
        match run_tunnel(socket, user_id, proxy_ip).await {
            Ok(_) => {
                // Normal closure
                let _ = server.close(Some(1000), Some("Tunnel completed"));
            }
            Err(e) => {
                console_error!("Tunnel error: {}", e);
                // Send appropriate error code based on error type
                let code = match e.kind() {
                    std::io::ErrorKind::ConnectionReset => 1006,  // Abnormal closure
                    std::io::ErrorKind::ConnectionAborted => 1001,  // Going away
                    std::io::ErrorKind::InvalidData => 1007,  // Invalid frame payload data
                    std::io::ErrorKind::TimedOut => 1001,  // Going away
                    _ => 1011,  // Internal error
                };
                let _ = server.close(Some(code), Some(&e.to_string()));
            }
        }
    });

    Response::from_websocket(client)
}

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
        // Quick validation with proper error codes
        if client_socket.read_u8().await? != 0 {
            return Err(Error::new(ErrorKind::InvalidData, "invalid version"));
        }
        
        if client_socket.read_bytes(16).await? != user_id {
            return Err(Error::new(ErrorKind::InvalidData, "invalid user id"));
        }

        // Skip addons efficiently
        let addon_len = client_socket.read_u8().await? as usize;
        if addon_len > 0 {
            let mut buf = vec![0u8; addon_len];
            client_socket.read_exact(&mut buf).await?;
        }

        let network_type = client_socket.read_u8().await?;
        let remote_port = client_socket.read_u16().await?;

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

        match network_type {
            protocol::NETWORK_TYPE_TCP => {
               for target in [vec![remote_addr], proxy_ip].concat() {
                    match process_tcp_outbound(&mut client_socket, &target, remote_port).await {
                        Ok(_) => {
                            // normal closed
                            return Ok(());
                        }
                        Err(e) => {
                            // connection reset
                            if e.kind() != ErrorKind::ConnectionReset {
                                return Err(e);
                            }

                            // continue to next target
                            continue;
                        }
                    }
                }

                Err(Error::new(ErrorKind::InvalidData, "no target to connect"))
            }
            protocol::NETWORK_TYPE_UDP => {
                process_udp_outbound(&mut client_socket, &remote_addr, remote_port).await
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
    ) -> Result<()> {
        let mut remote_socket = Socket::builder().connect(target, port).map_err(|e| {
            Error::new(ErrorKind::ConnectionAborted, format!("connect failed: {}", e))
        })?;

        if let Err(e) = remote_socket.opened().await {
            return Err(Error::new(ErrorKind::ConnectionReset, format!("socket not opened: {}", e)));
        }

        // Send response header
        client_socket.write(&[0u8, 0]).await?;

        // Use copy_bidirectional with proper error handling
        match copy_bidirectional(client_socket, &mut remote_socket).await {
            Ok(_) => Ok(()),
            Err(e) => {
                // Try to close remote socket
                let _ = remote_socket.close();
                Err(e)
            }
        }
    }

    async fn process_udp_outbound(
        client_socket: &mut WebSocketStream<'_>,
        _: &str,
        port: u16,
    ) -> Result<()> {
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
    }
}

mod ext {
    use std::io::Result;
    use tokio::io::AsyncReadExt;
    #[allow(dead_code)]
    pub trait StreamExt {
        async fn read_string(&mut self, n: usize) -> Result<String>;
        async fn read_bytes(&mut self, n: usize) -> Result<Vec<u8>>;
    }

    impl<T: AsyncReadExt + Unpin + ?Sized> StreamExt for T {
        async fn read_string(&mut self, n: usize) -> Result<String> {
            self.read_bytes(n).await.map(|bytes| {
                String::from_utf8(bytes).map_err(|e| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("invalid string: {}", e),
                    )
                })
            })?
        }

        async fn read_bytes(&mut self, n: usize) -> Result<Vec<u8>> {
            let mut buffer = vec![0u8; n];
            self.read_exact(&mut buffer).await?;

            Ok(buffer)
        }
    }
}

mod websocket {
    use std::io::{Error, ErrorKind, Result};
    use std::sync::atomic::{AtomicBool, Ordering};
    use bytes::{Bytes, BytesMut};
    use futures_util::StreamExt;
    use tokio::io::{AsyncRead, AsyncWrite};
    use worker::*;

    pub struct WebSocketStream<'a> {
        ws: &'a WebSocket,
        buffer: BytesMut,
        closed: AtomicBool,
        error_code: Option<u16>,
    }

    impl<'a> WebSocketStream<'a> {
        pub fn new(ws: &'a WebSocket, early_data: Option<Vec<u8>>) -> Self {
            let mut buffer = BytesMut::with_capacity(8192);
            if let Some(data) = early_data {
                buffer.extend_from_slice(&data);
            }
            Self {
                ws,
                buffer,
                closed: AtomicBool::new(false),
                error_code: None,
            }
        }

        #[inline]
        fn is_closed(&self) -> bool {
            self.closed.load(Ordering::Relaxed)
        }

        #[inline]
        fn set_closed(&self, error_code: Option<u16>) {
            self.closed.store(true, Ordering::Relaxed);
            if let Some(code) = error_code {
                // Only attempt to close if not already closed
                let _ = self.ws.close(Some(code), None);
            }
        }

        async fn process_message(&mut self) -> Result<Option<Bytes>> {
            if self.is_closed() {
                return Ok(None);
            }

            match self.ws.events().expect("stream error").next().await {
                Some(Ok(WebsocketEvent::Message(msg))) => {
                    if let Some(data) = msg.bytes() {
                        Ok(Some(Bytes::from(data)))
                    } else {
                        Ok(None)
                    }
                }
                 Some(Ok(WebsocketEvent::Close(e))) => {
                    // Handle close frame with proper error code
                    let code = e.and_then(|e| e.code);
                    self.set_closed(code);
                    Ok(None)
                }
                None => {
                    self.set_closed(Some(1000)); // Normal closure
                    Ok(None)
                }
                 Some(Err(e)) => {
                    self.set_closed(Some(1011)); // Internal error
                    Err(Error::new(ErrorKind::Other, e.to_string()))
                }
            }
        }
    }

    impl AsyncRead for WebSocketStream<'_> {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<Result<()>> {
            use std::task::Poll;

            if self.is_closed() {
                return Poll::Ready(Ok(()));
            }

             // Use buffered data first
            if !self.buffer.is_empty() {
                let to_read = std::cmp::min(buf.remaining(), self.buffer.len());
                buf.put_slice(&self.buffer.split_to(to_read));
                return Poll::Ready(Ok(()));
            }

             // Then try to get new data
             match futures_util::ready!(std::pin::Pin::new(&mut futures_util::future::poll_fn(|cx| {
                Box::pin(self.process_message()).as_mut().poll(cx)
            }))) {
                Ok(Some(data)) => {
                    buf.put_slice(&data);
                    Poll::Ready(Ok(()))
                }
                 Ok(None) => {
                     if !self.is_closed() {
                       self.set_closed(Some(1000));
                    }
                    Poll::Ready(Ok(()))
                }
                Err(e) => {
                     self.set_closed(Some(1011));
                     Poll::Ready(Err(e))
                }
            }
        }
    }

    impl AsyncWrite for WebSocketStream<'_> {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<Result<usize>> {
            use std::task::Poll;

            if self.is_closed() {
                return Poll::Ready(Ok(0));
            }

            match self.ws.send_with_bytes(buf) {
                Ok(_) => Poll::Ready(Ok(buf.len())),
                Err(e) => {
                     self.set_closed(Some(1011));
                     Poll::Ready(Err(Error::new(ErrorKind::Other, e.to_string())))
                }
            }
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<()>> {
              if !self.is_closed() {
                  self.set_closed(Some(1000));  // Normal closure
                }
            std::task::Poll::Ready(Ok(()))
        }
    }
}