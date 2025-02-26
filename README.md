# Workers Tunnel

[![Deploy to Cloudflare Workers](https://deploy.workers.cloudflare.com/button)](https://deploy.workers.cloudflare.com/?url=https://github.com/zhu327/workers-tunnel/tree/main)

**Workers Tunnel** is an edge network tunnel powered by Cloudflare Workers, allowing traffic to be securely proxied through the Cloudflare network. This solution is particularly useful for users who need reliable connectivity while bypassing restrictions.

## Features

- **Cloudflare Workers Integration** – Seamless deployment on Cloudflare's global edge network.
- **V2ray/Xray Client Support** – Optimized for use with the [V2ray-core](https://github.com/v2fly/v2ray-core) or [Xray-core](https://github.com/XTLS/Xray-core) tunnel client.
- **Secure and Configurable** – Easily customize the setup with Cloudflare Worker variables settings.
- **Global Availability** – Deployed and distributed across Cloudflare's network.

## Limitations

Due to Cloudflare Workers' constraints:
- **UDP proxying is not supported.**
- **Direct connections to Cloudflare IPs without proxy are restricted.**

## Usage of Variables

The configuration variables are defined in `wrangler.toml` and control how the tunnel operates. Below are detailed explanations of each variable:

### `USER_ID`

- **Purpose**: Used to authenticate with the V2Ray/Xray client.
- **Usage**: Instead of hardcoding it in `wrangler.toml`, store it securely in Cloudflare Secrets for better security.
- **Example Configuration in Secrets**:
  ```sh
  wrangler secret put USER_ID
  ```
  When prompted, enter the UUID securely.

### `PROXY_IP`

- **Purpose**: Specifies the proxy server address to connect to the Cloudflare Network.
- **Reason**: Cloudflare Workers prohibit direct socket connections to Cloudflare IPs, so a proxy is needed to relay traffic.
- **Example Configuration**:
  ```toml
  PROXY_IP = "gh.proxy.farelra.my.id"
  ```
  Change the value based on your proxy server’s domain.

### `FALLBACK_SITE`

- **Purpose**: Determines the fallback website when the worker is accessed outside of the V2Ray/Xray client.
- **Reason**: Ensures that users who mistakenly visit the worker URL directly see a default page instead of an error.
- **Example Configuration**:
  ```toml
  FALLBACK_SITE = "farelra.my.id"
  ```
  Replace it with your own fallback site.

### `SHOW_URI`

- **Purpose**: Controls whether the worker displays the VLESS Template URI at the `/{UUID}` path.
- **Reason**: This allows quick sharing of connection details but can be disabled for privacy.
- **Example Configuration**:
  ```toml
  SHOW_URI = "true"
  ```
  Set to `false` if you do not want the URI to be exposed.

## Installation & Deployment

### Prerequisites
- A **Cloudflare Workers account**.
- A **Cloudflare domain** (optional but recommended).
- A **UUID** for authentication.

To set up this project using Cloudflare Workers, follow these steps:

1. **Install Dependencies** (Choose one based on your package manager):

   ```sh
   npm init cloudflare my-project workers-tunnel
   # or
   yarn create cloudflare my-project workers-tunnel
   # or
   pnpm create cloudflare my-project workers-tunnel
   ```

2. **Modify Configuration**:

   - Update `wrangler.toml` with your custom values.
   - Store `USER_ID` securely using Cloudflare Secrets.

3. **Deploy the Worker**:

   ```sh
   npm run deploy
   ```

## Example Xray Configuration

To configure the Xray client for this tunnel, use the following example (replace `your.domain.workers.dev` with your Cloudflare Workers domain):

```json
{
  "log": {
    "loglevel": "warning"
  },
  "inbounds": [
    {
      "port": 1080,
      "protocol": "socks",
      "sniffing": {
        "enabled": true,
        "destOverride": [
          "http",
          "tls"
        ]
      }
    }
  ],
  "outbounds": [
    {
      "settings": {
        "vnext": [
          {
            "port": 443,
            "users": [
              {
                "id": "c55ba35f-12f6-436e-a451-4ce982c4ec1c",
                "encryption": "none"
              }
            ],
            "address": "your.domain.workers.dev"
          }
        ]
      },
      "protocol": "vless",
      "streamSettings": {
        "network": "ws",
        "tlsSettings": {
          "serverName": "your.domain.workers.dev",
          "fingerprint": "chrome"
        },
        "wsSettings": {
          "headers": {
            "Host": "your.domain.workers.dev"
          },
          "path": "/ws?ed=2560"
        },
        "security": "tls"
      }
    },
    {
      "protocol": "freedom",
      "tag": "direct"
    }
  ],
  "routing": {
    "domainStrategy": "IPIfNonMatch",
    "rules": [
      {
        "type": "field",
        "outboundTag": "direct",
        "ip": [
          "geoip:private",
          "geoip:cloudflare"
        ]
      }
    ]
  }
}
```

## WebAssembly Support

Workers Tunnel is built using Rust and compiled into WebAssembly. Ensure that all dependencies support the `wasm32-unknown-unknown` target.

## Troubleshooting & Issues

If you encounter any problems:
- Check Cloudflare Workers logs for debugging (`wrangler tail`).
- Verify your domain settings and routing configurations.
- Open an issue on the [`workers-rs`](https://github.com/cloudflare/workers-rs) repository for Rust-related issues.

## Documentation & Resources

- **Cloudflare Workers API**: [Official Docs](https://developers.cloudflare.com/workers/)
- **Rust WebAssembly in Workers**: [Read More](https://developers.cloudflare.com/workers/runtime-apis/webassembly/rust/)
- **V2Ray/Xray Core**: [GitHub Repository](https://github.com/XTLS/Xray-core)
- **Traffic Routing Rules**: [Loyalsoldier Rules](https://github.com/Loyalsoldier/v2ray-rules-dat)

## Contributing

Contributions are welcome! Feel free to submit pull requests or report issues to improve the project.

## License

This project is licensed under the MIT License. See the `LICENSE` file for details.
