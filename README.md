# Edge tunnel on Cloudflare Workers

A lightweight edge network tunnel implemented using Cloudflare Workers, leveraging the VLESS protocol for secure and efficient data transfer. This project provides a lightweight and efficient way to create a tunnel using Cloudflare Workers. It leverages the power of Cloudflare's edge network to provide a fast and reliable connection.

## Features

- **VLESS Protocol:** Utilizes the modern VLESS protocol, known for its speed and security, for tunnel communication.
- **Cloudflare Workers:** Built on Cloudflare Workers, providing a serverless and scalable solution with global edge network performance.
- **Easy Deployment:** Simple deployment process with the provided template and Cloudflare Workers tooling.
- **Rust Implementation:**  Written in Rust for performance, safety, and efficiency.

## Getting Started

[![Deploy to Cloudflare Workers](https://deploy.workers.cloudflare.com/button)](https://deploy.workers.cloudflare.com/?url=https://github.com/FarelRA/workers-tunnel/tree/main)

### Prerequisites

- A Cloudflare account.
- `npm`, `yarn`, or `pnpm` package manager installed.
- `wrangler` CLI installed (Cloudflare's command-line tool for Workers).

### Quick Start

1. **Create a new project:** Use one of the following commands to create a new project based on this template:

   ```sh
   $ npm init cloudflare my-project workers-tunnel
   # or
   $ yarn create cloudflare my-project workers-tunnel
   # or
   $ pnpm create cloudflare my-project workers-tunnel
   ```

   This command will create a new directory `my-project` with the template files.

2. **Navigate to the project directory:**

   ```sh
   cd my-project
   ```

3. **Configure `wrangler.toml`:**
   - Open the `wrangler.toml` file.
   -  Locate the `[vars]` section and set the `USER_ID` to your desired UUID (required for vless authentication).

     ```toml
     [vars]
     USER_ID = "your-uuid-here" # Replace with your UUID
     ```

4. **Development:** Start the development server to test your worker:

   ```sh
   npm run dev
   ```

5. **Deployment:** Deploy your Worker to Cloudflare's edge network:

   ```sh
   npm run deploy
   ```
   
Refer to the latest `worker` crate documentation for more information: [https://docs.rs/worker](https://docs.rs/worker)

## Usage

This template provides a starting point for your edge tunnel. The main entrypoint for requests is `src/lib.rs`. Feel free to add additional Rust code and modules to customize and extend the functionality.

### Development Workflow

- Use `npm run dev` to start a local development server with hot-reloading for a faster and efficient development experience.
- Use `npm run deploy` to push your changes to the Cloudflare edge.

### Client Configuration

When configuring your VLESS client, ensure that you:
-   Set the server address to your Cloudflare worker domain.
-   The tunnel is configured to work with the VLESS protocol using WebSockets. 
-   Set the correct `USER_ID` in your client to match the value you configured in `wrangler.toml`.
-   Set the `Host` header to your worker domain in the WebSocket settings.

## Important Notes

*   **WebAssembly Compilation:** `workers-rs` requires all code and dependencies to compile to the `wasm32-unknown-unknown` target. Be sure to consider this when adding dependencies. More info on [workers-rs](https://github.com/cloudflare/workers-rs).
*  **UDP Proxy limitation:** Cloudflare Workers does not support UDP proxy.

## Troubleshooting

- **Issues with the `worker` crate:** Please open an issue on the [upstream project issue tracker](https://github.com/cloudflare/workers-rs/issues).
- **Deployment errors:** Check Cloudflare's dashboard for detailed logs.
- **Configuration:** Ensure your configuration in `wrangler.toml` and client matches.

## Contributing

Contributions are welcome! If you find any issues or have suggestions for improvements, please open an issue or submit a pull request.

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.