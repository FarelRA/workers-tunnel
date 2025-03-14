// src/lib.rs
use crate::proxy::{parse_early_data, parse_user_id, run_tunnel, TunnelConfig};
use crate::websocket::WebSocketStream;
use wasm_bindgen::JsValue;
use worker::*;
use std::sync::Arc;

#[event(fetch)]
async fn main(req: Request, env: Env, _: Context) -> Result<Response> {
    let config = Arc::new(TunnelConfig {
        user_id: parse_user_id(&env.var("USER_ID")?.to_string()),
        proxy_ip: env
            .var("PROXY_IP")?
            .split_ascii_whitespace()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect(),
        fallback_site: env
            .var("FALLBACK_SITE")
            .unwrap_or(JsValue::from_str("").into())
            .to_string(),
        show_uri: env
            .var("SHOW_URI")?
            .parse()
            .unwrap_or(false),
    });

    let is_websocket = req
        .headers()
        .get("Upgrade")?
        .map(|up| up.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false);
    let should_fallback = !is_websocket;

    if should_fallback && config.show_uri {
        let path = req.path();
        if path.contains(config.user_id.to_hex()) {
            let host_str = req.url()?.host_str().ok_or("Invalid host")?;
            return Response::ok(format!(
                "vless://{uuid}@{host}:443?encryption=none&security=tls&sni={host}&fp=chrome&type=ws&host={host}&path=ws#workers-tunnel",
                uuid = config.user_id.to_hex(),
                host = host_str
            ));
        }
    }

    if should_fallback && !config.fallback_site.is_empty() {
        let base_url = Url::parse(&config.fallback_site)?;
        let original_url = req.url()?;
        
        // Construct the new URL with fallback origin and original path/query
        let mut new_url = Url::parse(&base_url.origin().ascii_serialization())?;
        new_url.set_path(original_url.path());
        if let Some(query) = original_url.query() {
            new_url.set_query(Some(query));
        }
        
        let req = Fetch::Url(new_url);
        return req.send().await;
    }

    // ready early data
    let early_data = req.headers().get("sec-websocket-protocol")?;
    let early_data = parse_early_data(early_data)?;

    let (client, server) = req.accept_websocket()?; // Updated for worker 0.6
    server.accept()?;

    wasm_bindgen_futures::spawn_local(async move {
        // create websocket stream
        let socket = WebSocketStream::new(
            &server,
            server.events().expect("WebSocket stream failed"),
            early_data,
        );

        if let Err(err) = run_tunnel(socket, Arc::clone(&config)).await {
            console_error!("Error: {}", err);
            _ = server.close_with_code(1003, "Invalid request");
        }
    });

    Response::from_websocket(client)
}

// proxy.rs
mod proxy {
    use std::sync::Arc;
    use std::io::{Error, ErrorKind, Result};
    use std::net::{Ipv4Addr, Ipv6Addr};

    use crate::ext::StreamExt;
    use crate::protocol;
    use crate::websocket::WebSocketStream;
    use hex::FromHexError;

    pub struct TunnelConfig {
        pub user_id: Vec<u8>,
        pub proxy_ip: Vec<String>,
        pub fallback_site: String,
        pub show_uri: bool,
    }

    pub fn parse_early_data(data: Option<String>) -> Result<Option<Vec<u8>>> {
        if let Some(data) = data {
            if !data.is_empty() {
                let s = data.replace('+', "-").replace('/', "_").replace('=', "");
                return decode_config(&s, base64::URL_SAFE_NO_PAD)
                    .map(|d| Some(d))
                    .map_err(|e| Error::new(ErrorKind::Other, e.to_string()));
            }
        }
        Ok(None)
    }

    pub fn parse_user_id(user_id: &str) -> Result<Vec<u8>, FromHexError> {
        hex::decode(user_id)
    }

    pub async fn run_tunnel(
        mut client_socket: WebSocketStream<'_>,
        config: Arc<TunnelConfig>,
    ) -> Result<()> {
        // read version
        if client_socket.read_u8().await? != protocol::VERSION {
            return Err(Error::new(ErrorKind::InvalidData, "Invalid version"));
        }

        let received_user_id = client_socket.read_bytes(16).await?;
        if received_user_id != config.user_id {
            return Err(Error::new(ErrorKind::InvalidData, "Invalid user ID"));
        }

        let _ = client_socket.read_bytes(client_socket.read_u8().await? as usize).await?;

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
                Ipv4Addr::from(client_socket.read_u32().await?).to_string()
            }
            protocol::ADDRESS_TYPE_IPV6 => {
                format!("[{}]", Ipv6Addr::from(client_socket.read_u128().await?))
            }
            _ => return Err(Error::new(ErrorKind::InvalidData, "Invalid address type")),
        };

        // process outbound
        match network_type {
            protocol::NETWORK_TYPE_TCP => {
                for target in std::iter::once(&remote_addr).chain(config.proxy_ip.iter().map(|s| s.as_str())) {
                    if let Ok(()) = process_tcp_outbound(&mut client_socket, target, remote_port).await {
                        return Ok(());
                    }
                }
                Err(Error::new(ErrorKind::ConnectionRefused, "No valid proxy target"))
            }
            protocol::NETWORK_TYPE_UDP => {
                process_udp_outbound(&mut client_socket, remote_port).await
            }
            _ => Err(Error::new(ErrorKind::InvalidInput, "Unsupported network type")),
        }
    }

    async fn process_tcp_outbound(
        client_socket: &mut WebSocketStream<'_>,
        target: &str,
        port: u16,
    ) -> Result<()> {
        let mut remote_socket = Socket::connect(target, port).await?; // Updated API
        client_socket.write(&protocol::RESPONSE).await?;
        tokio::io::copy_bidirectional(client_socket, &mut remote_socket).await?;
        Ok(())
    }

    async fn process_udp_outbound(
        client_socket: &mut WebSocketStream<'_>,
        port: u16,
    ) -> Result<()> {
        // check port (only support dns query)
        if port != 53 {
            return Err(Error::new(ErrorKind::InvalidInput, "UDP only supports DNS (port 53)"));
        }

        client_socket.write(&protocol::RESPONSE).await?;

        // forward data
        loop {
            let length = client_socket.read_u16().await?;
            let packet = client_socket.read_bytes(length as usize).await?;

            let request = Request::new_with_init(
                "https://1.1.1.1/dns-query",
                &RequestInit::new()
                    .method(Method::Post)
                    .header("Content-Type", "application/dns-message")
                    .body(packet.into())
                    .timeout(400),
            )?;

            let response = Fetch::Request(request).send().await?;
            let data = response.bytes().await?;

            if data.is_empty() {
                continue;
            }

            client_socket.write_u16(data.len() as u16).await?;
            client_socket.write_all(&data).await?;
        }
    }
}
