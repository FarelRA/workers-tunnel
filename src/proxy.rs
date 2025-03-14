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
