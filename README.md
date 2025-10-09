# p3-standalone

This repository contains an internal testing tool, originally moved from `paladin-bladesmith/paladin-solana` to its own separate repository for improved modularity and development.

## Repository Structure

- **p3-standalone/**: Main source code for the testing tool.
- **jito-protos/**: Protocol buffer definitions and related Rust code.
- **paladin-programs/**: Submodules for dependencies required by `p3-standalone`.

## Usage

### Basic Usage

To run the `p3-standalone` binary, use the following sample command:

```bash
cargo run --bin p3-standalone -- --rpc-servers <RPC_SERVERS> --websocket-servers <WEBSOCKET_SERVERS> --p3-addr <NORMAL_P3_PORT_ADDR> --p3-mev-addr <MEV_P3_PORT_ADDR> --grpc-bind-ip <BLOCK_ENGINE_ADDR>
```

### Example with Default Values

```bash
cargo run --bin p3-standalone -- \
  --rpc-servers http://127.0.0.1:8899 \
  --websocket-servers ws://127.0.0.1:8900 \
  --p3-addr 127.0.0.1:4819 \
  --p3-mev-addr 127.0.0.1:4820 \
  --grpc-bind-ip 127.0.0.1:5999
```

### Command Line Options

For all available options, run:

```bash
cargo run --bin p3-standalone -- --help
```

Key configuration options include:

- `--p3-addr`: P3 QUIC server address for regular packets (default: 127.0.0.1:4819)
- `--p3-mev-addr`: P3 QUIC server address for MEV packets (default: 127.0.0.1:4820)
- `--rpc-servers`: Space-separated list of Solana RPC servers
- `--websocket-servers`: Space-separated list of corresponding WebSocket servers
- `--grpc-bind-ip`: Block Engine gRPC server address (default: 127.0.0.1:5999)
- `--identity-keypair`: Path to identity keypair file (generates new if not provided)
- `--access-token-ttl-secs`: Access token validity duration (default: 1800s)
- `--refresh-token-ttl-secs`: Refresh token validity duration (default: 180000s)
- `--logs`: Directory for hourly log files

### Environment Configuration

The tool supports `.env` files for environment-based configuration. Place a `.env` file in the working directory to set environment variables.

## Development Notes

- This tool is intended for internal testing and development purposes.
- The `paladin-programs` directory is included as a submodule for dependency management.
