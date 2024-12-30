use crate::proxy::{parse_early_data, parse_user_id, run_tunnel};
use crate::websocket::WebSocketStream;
use worker::*;

#[event(fetch)]
async fn main(req: Request, env: Env, _: Context) -> Result<Response> {
    use worker::console_error;

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
    let should_fallback = match req.headers().get("Upgrade")? {
        Some(up) => up != *"websocket",
        None => true,
    };
    if should_fallback && !fallback_site.is_empty() {
        let req = Fetch::Url(Url::parse(&fallback_site)?);
        return req.send().await;
    }

    // ready early data
    let early_data = req.headers().get("sec-websocket-protocol")?;
    let early_data = parse_early_data(early_data)?;

    // Accept / handle a websocket connection
    let WebSocketPair { client, server } = WebSocketPair::new()?;
    server.accept()?;

    let user_id_clone = user_id.clone();
    let proxy_ip_clone = proxy_ip.clone();
    wasm_bindgen_futures::spawn_local(async move {
        // create websocket stream
        let socket = WebSocketStream::new(
            &server,
            server.events().expect("could not open stream"),
            early_data,
        );

        // create a Result to track if the tunnel process completed or not, for closing correctly
        let tunnel_result = run_tunnel(socket, user_id_clone, proxy_ip_clone).await;

        // log the result
        if let Err(err) = &tunnel_result {
            console_error!("error: {}", err);
        }

        let close_code = match tunnel_result {
            Ok(_) => Some(1000),  // Normal Closure
            Err(_) => Some(1003), // Invalid request or other error
        };

        let close_reason = match tunnel_result {
            Ok(_) => Some("normal close"),
            Err(_) => Some("invalid request"),
        };

        // close the socket
        if let Err(err) = server.close(close_code, close_reason) {
            console_error!("error closing websocket: {}", err);
        }
    });

    Response::from_websocket(client)
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

    use crate::ext::StreamExt;
    use crate::protocol;
    use crate::websocket::WebSocketStream;
    use base64::{decode_config, URL_SAFE_NO_PAD};
    use tokio::io::{copy_bidirectional, AsyncReadExt, AsyncWriteExt};
    use worker::*;

    pub fn parse_early_data(data: Option<String>) -> Result<Option<Vec<u8>>> {
        if let Some(data) = data {
            if data.is_empty() {
                return Ok(None);
            }
            let mut s = data.into_bytes();
            for byte in s.iter_mut() {
                match *byte {
                    b'+' => *byte = b'-',
                    b'/' => *byte = b'_',
                    b'=' => {
                        s.pop();
                        continue;
                    }
                    _ => {}
                }
            }
            match decode_config(s, URL_SAFE_NO_PAD) {
                Ok(early_data) => return Ok(Some(early_data)),
                Err(err) => return Err(Error::new(ErrorKind::Other, err.to_string())),
            }
        }
        Ok(None)
    }

    pub fn parse_user_id(user_id: &str) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(user_id.len() / 2);
        let mut chars = user_id.chars();

        while let (Some(high), Some(low)) = (chars.next(), chars.next()) {
            if let (Some(h), Some(l)) = (high.to_digit(16), low.to_digit(16)) {
                bytes.push(((h << 4) | l) as u8);
            }
        }
        bytes
    }

    pub async fn run_tunnel(
        mut client_socket: WebSocketStream<'_>,
        user_id: Vec<u8>,
        proxy_ip: Vec<String>,
    ) -> Result<()> {
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

        // process outbound
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
        // connect to remote socket
        let mut remote_socket = match Socket::builder().connect(target, port) {
            Ok(socket) => socket,
            Err(e) => {
                return Err(Error::new(
                    ErrorKind::ConnectionAborted,
                    format!("connect to remote failed: {}", e),
                ))
            }
        };
        // check remote socket
        if let Err(e) = remote_socket.opened().await {
            _ = remote_socket.close();
            return Err(Error::new(
                ErrorKind::ConnectionReset,
                format!("remote socket not opened: {}", e),
            ));
        }

        // send response header
        if let Err(e) = client_socket.write(&protocol::RESPONSE).await {
            _ = remote_socket.close();
            return Err(Error::new(
                ErrorKind::ConnectionAborted,
                format!("send response header failed: {}", e),
            ));
        }

        // forward data
        let copy_result = copy_bidirectional(client_socket, &mut remote_socket).await;
        _ = remote_socket.shutdown().await;
        if let Err(e) = copy_result {
            if e.kind() != ErrorKind::BrokenPipe && e.kind() != ErrorKind::ConnectionAborted {
                return Err(Error::new(
                    ErrorKind::ConnectionAborted,
                    format!("forward data between client and remote failed: {}", e),
                ));
            }
        }

        Ok(())
    }

    async fn process_udp_outbound(
        client_socket: &mut WebSocketStream<'_>,
        _: &str,
        port: u16,
    ) -> Result<()> {
        // check port (only support dns query)
        if port != 53 {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "not supported udp proxy yet",
            ));
        }

        // send response header
        if let Err(e) = client_socket.write(&protocol::RESPONSE).await {
            return Err(Error::new(
                ErrorKind::ConnectionAborted,
                format!("send response header failed: {}", e),
            ));
        }

        // forward data
        let mut subrequest_count = 0;
        loop {
            // read packet length
            let length = client_socket.read_u16().await;
            if let Err(_) = length {
                return Ok(());
            }
            let length = length.unwrap();
            if length == 0 {
                return Ok(());
            }
            // read dns packet
            let packet = match client_socket.read_bytes(length as usize).await {
                Ok(packet) => packet,
                Err(_) => return Ok(()),
            };

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
            let mut response = match Fetch::Request(request).send().await {
                Ok(response) => response,
                Err(e) => {
                    console_error!("send DNS-over-HTTP request failed: {}", e);
                    continue;
                }
            };

            // read response
            let data = match response.bytes().await {
                Ok(data) => data,
                Err(e) => {
                    console_error!("DNS-over-HTTP response body error: {}", e);
                    continue;
                }
            };

            // write response
            if let Err(e) = client_socket.write_u16(data.len() as u16).await {
                console_error!("write udp length failed: {}", e);
                continue;
            }
            if let Err(e) = client_socket.write_all(&data).await {
                console_error!("write udp data failed: {}", e);
                continue;
            }

            subrequest_count += 1;
            if subrequest_count > 10 {
                console_error!("Too many subrequest, stopping DNS resolution");
                return Ok(());
            }
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
    use futures_util::Stream;
    use std::{
        io::{Error, ErrorKind, Result},
        pin::Pin,
        task::{Context, Poll},
    };

    use bytes::{BufMut, BytesMut};
    use pin_project::pin_project;
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
    use worker::{console_error, EventStream, WebSocket, WebsocketEvent}; // Add console_error here

    #[pin_project]
    pub struct WebSocketStream<'a> {
        ws: &'a WebSocket,
        #[pin]
        stream: EventStream<'a>,
        buffer: BytesMut,
        is_closed: bool,
    }

    impl<'a> WebSocketStream<'a> {
        pub fn new(
            ws: &'a WebSocket,
            stream: EventStream<'a>,
            early_data: Option<Vec<u8>>,
        ) -> Self {
            let mut buffer = BytesMut::new();
            if let Some(data) = early_data {
                buffer.put_slice(&data)
            }

            Self {
                ws,
                stream,
                buffer,
                is_closed: false,
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
            if *this.is_closed {
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
                    Poll::Ready(Some(Err(e))) => {
                        *this.is_closed = true;
                        return Poll::Ready(Err(Error::new(ErrorKind::Other, e.to_string())));
                    }
                    Poll::Ready(None) => {
                        *this.is_closed = true;
                        return Poll::Ready(Ok(()));
                    } // None or Close event, return Ok to indicate stream end
                    Poll::Ready(Some(Ok(WebsocketEvent::Close(_)))) => {
                        *this.is_closed = true;
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
            self.ws
                .send_with_bytes(buf)
                .map(|_| Poll::Ready(Ok(buf.len())))
                .map_err(|e| Poll::Ready(Err(Error::new(ErrorKind::Other, e.to_string()))))?
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<()>> {
            if !self.is_closed {
                if let Err(e) = self.ws.close(None, Some("normal close")) {
                    // we don't really care if we failed to close it, because it means it was already closed.
                    console_error!("failed to close websocket in shutdown {}", e);
                }
            }
            Poll::Ready(Ok(()))
        }
    }
}