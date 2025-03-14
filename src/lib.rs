mod proxy;
mod ext;
mod websocket;

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
