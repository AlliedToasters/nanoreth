# HyperEVM Mainnet Sync Guide

Complete guide to syncing a nanoreth mainnet archive node, including the init-state generation process required for mainnet. Based on lessons learned syncing to block 28M+ (Feb 2026).

> **Prerequisite**: Read the [Testnet Sync Guide](TESTNET_SYNC_GUIDE.md) first. It covers the architecture, pipeline stages, and troubleshooting in detail. This guide focuses on mainnet-specific differences.

## Table of Contents

- [Key Differences from Testnet](#key-differences-from-testnet)
- [Step 1: Start hl-node for mainnet](#step-1-start-hl-node-for-mainnet)
- [Step 2: Build nanoreth (with spot metadata fix)](#step-2-build-nanoreth-with-spot-metadata-fix)
- [Step 3: Generate mainnet init-state](#step-3-generate-mainnet-init-state)
- [Step 4: Initialize the database](#step-4-initialize-the-database)
- [Step 5: Start nanoreth](#step-5-start-nanoreth)
- [Step 6: (Optional) Backfill historical blocks](#step-6-optional-backfill-historical-blocks)
- [Mainnet-Specific Troubleshooting](#mainnet-specific-troubleshooting)
- [Docker Compose (running both networks)](#docker-compose-running-both-networks)

---

## Key Differences from Testnet

| | Testnet | Mainnet |
|---|---------|---------|
| **Chain ID** | 998 | 999 |
| **hl-visor URL** | `binaries.hyperliquid-testnet.xyz/Testnet` | `binaries.hyperliquid.xyz/Mainnet` |
| **S3 bucket** | `hl-testnet-evm-blocks` | `hl-mainnet-evm-blocks` |
| **Public genesis repo** | Yes (`sprites0/hl-testnet-genesis`) | **No** — must generate from ABCI state |
| **Init-state required** | Yes (sync from block 34M) | **Yes** — must generate yourself |
| **Block count** | ~46M | ~28M (as of Feb 2026) |
| **visor.json** | `{"chain": "Testnet"}` | `{"chain": "Mainnet"}` |
| **Default RPC port** | 8545 | 8547 (to avoid conflict if running both) |
| **Gossip ports** | 4001/4002 (host network) | 4001/4002 (bridge network if co-located with testnet) |

### Why init-state is required

Nanoreth's pseudo peer downloads headers via a P2P session that times out after ~23 seconds. The headers stage doesn't checkpoint partway through — if the session disconnects, all downloaded headers are discarded. At ~1M headers per 23-second session, syncing 28M headers from genesis is impossible.

The solution is init-state: seed the database with account state at a recent block, so the node only needs to sync the small gap between the init-state block and chain tip.

> **Unlike testnet**, there is no public genesis repository for mainnet. You must generate the init-state files yourself from hl-node's ABCI state snapshot.

---

## Step 1: Start hl-node for mainnet

Same process as testnet (see [Testnet Guide Step 1](TESTNET_SYNC_GUIDE.md#step-1-start-hl-node-do-this-first)), but with mainnet URLs:

```sh
# Download mainnet hl-visor
curl https://binaries.hyperliquid.xyz/Mainnet/hl-visor > ~/hl-visor-mainnet
chmod a+x ~/hl-visor-mainnet
echo '{"chain": "Mainnet"}' > ~/visor-mainnet.json

# Start (adjust paths to avoid conflict with testnet)
HL_HOME=~/hl-mainnet nohup ~/hl-visor-mainnet run-non-validator \
  --serve-evm-rpc --disable-output-file-buffering \
  > ~/hl-visor-mainnet.log 2>&1 &
```

### Wait for ABCI state snapshots

hl-node must fully bootstrap before you can generate init-state. Monitor progress:

```sh
# Check for ABCI state snapshots (one written every ~10k L1 rounds)
ls ~/hl-mainnet/data/periodic_abci_states/

# Check for EVM block output (means node is fully synced)
ls ~/hl-mainnet/data/evm_block_and_receipts/hourly/
```

**Important**: You need at least one ABCI state snapshot AND the corresponding EVM RocksDB. hl-node writes periodic ABCI states to `data/periodic_abci_states/{date}/{round}.rmp` and maintains the EVM state database at `hyperliquid_data/evm_db_hub_slow/EvmState/`.

Bootstrap time varies: 30 minutes to several hours depending on peer availability and network speed. Mainnet has more peers than testnet, so bootstrap is typically faster.

---

## Step 2: Build nanoreth (with spot metadata fix)

### The spot metadata problem

Mainnet has ERC20 spot tokens that require metadata lookups during block processing. The upstream code calls `erc20_contract_to_spot_token(chain_id).unwrap()` which **panics** when the Hyperliquid API returns HTTP 429 (rate limit). Additionally, non-spot contracts cause an infinite retry loop.

You need the `fix/spot-meta-429` branch (or equivalent fixes):

```sh
git clone https://github.com/AlliedToasters/nanoreth.git
cd nanoreth
git checkout fix/spot-meta-429  # or apply the fixes manually
```

The fix adds:
- **Negative cache**: Known non-spot contracts are cached to avoid repeated API calls
- **Fetch-once semantics**: Spot metadata is fetched at most once per node lifetime
- **Graceful fallback**: API failures log a warning instead of panicking

Build:
```sh
make install
# or
docker build -t nanoreth:spot-429-fix .
```

---

## Step 3: Generate mainnet init-state

This is the most complex mainnet-specific step. You need to generate two files:
- `{block}.jsonl` — Account state (addresses, balances, nonces, code, storage)
- `{block}.rlp` — RLP-encoded HlHeader

### 3a. Build the evm-init tool

The [evm-init tool](https://github.com/sprites0/evm-init) generates init-state files, but needs modifications for mainnet:

1. **Type mismatch fix**: The upstream tool uses `reth_primitives::SealedBlock` for deserialization, but HL blocks are serialized with nanoreth's custom `reth_compat::SealedBlock` types. You must replace the types in `src/types.rs` with custom types matching nanoreth's format (see [Type Definitions](#type-definitions-for-evm-init) below).

2. **Direct DB mode**: The ABCI state deserialization fails on mainnet's `NoEvmDb` format. Add `--db-path` and `--block-number` CLI arguments to bypass ABCI deserialization and read the EVM RocksDB directly.

3. **Read-only DB access**: The RocksDB files are owned by root (created by Docker). Open with `DB::open_for_read_only()` instead of `DB::open()`.

```sh
git clone https://github.com/sprites0/evm-init.git
cd evm-init
# Apply the modifications described below, then:
cargo build --release
```

### 3b. Find the block number

Extract the latest EVM block number from an ABCI state snapshot:

```python
python3 -c "
import struct, sys

# Read just enough of the ABCI state to find the EVM block number
# The block number is embedded in the EvmBlock::Reth115 header
with open('path/to/periodic_abci_states/{date}/{round}.rmp', 'rb') as f:
    data = f.read()

# Search for the Reth115 enum variant marker followed by header data
# This is fragile — a proper msgpack parser is better
import msgpack
# Due to the large file size and complex nested structure,
# it's easier to use the EVM block files directly:
"

# Simpler: check the latest hourly EVM block file
ls ~/hl-mainnet/data/evm_block_and_receipts/hourly/ | sort | tail -1
# Then check the latest block in that directory
```

Or use Python to parse an ABCI state (slower but definitive):

```python
python3 << 'EOF'
import msgpack, sys

# Parse just the header portion — this loads the entire file into memory (~1GB)
with open("path/to/abci_state.rmp", "rb") as f:
    # Use raw=False to get strings, strict_map_key=False for byte keys
    state = msgpack.unpackb(f.read(), raw=False, strict_map_key=False)

evm = state["exchange"]["hyper_evm"]
block = evm["latest_block2"]
# block is {"Reth115": {"header": {"hash": ..., "header": {"number": ...}}, "body": ...}}
header = block["Reth115"]["header"]["header"]
block_number = header["number"]
block_hash = block["Reth115"]["header"]["hash"]
print(f"Block: {block_number}")
print(f"Hash:  0x{block_hash.hex()}")
EOF
```

### 3c. Run evm-init

```sh
./target/release/evm-init \
  --db-path ~/hl-mainnet/hyperliquid_data/evm_db_hub_slow/EvmState \
  --block-number <BLOCK_NUMBER> \
  --bucket hl-mainnet-evm-blocks
```

This will:
1. Download the block from S3 to get receipts and the header
2. Open the EVM RocksDB (read-only) to read all accounts, contracts, and storage
3. Write `{block}.jsonl` (accounts) and `{block}.rlp` (header)
4. Print the `reth-hl init-state` command to run

**Expected output** (mainnet, Feb 2026):
```
Loaded 123,809 contracts
Processed 1,263,013 accounts total
Generated 27975257.jsonl and 27975257.rlp
```

The JSONL file will be ~13 GB. Generation takes ~3 minutes.

### 3d. Stop hl-node before reading RocksDB (if needed)

If hl-node is actively writing to the EVM RocksDB, you may get inconsistent reads. Either:
- **Stop hl-node** temporarily while running evm-init
- **Use a periodic ABCI state checkpoint directory** — these contain point-in-time snapshots at `hyperliquid_data/evm_db_hub_slow/checkpoint/{round}/EvmState/`

### Type definitions for evm-init

Replace the block-related types in `src/types.rs` with these custom types that match nanoreth's serialization format:

```rust
use alloy_consensus::{Header, TxEip1559, TxEip2930, TxEip4844, TxEip7702, TxLegacy};
use alloy_primitives::{Address, BlockHash, Bytes, Log, Signature, B256};
use serde::{Deserialize, Serialize};

// Custom types matching nanoreth's reth_compat serialization
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Transaction {
    Legacy(TxLegacy),
    Eip2930(TxEip2930),
    Eip1559(TxEip1559),
    Eip4844(TxEip4844),
    Eip7702(TxEip7702),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionSigned {
    signature: Signature,
    transaction: Transaction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealedHeader {
    pub hash: BlockHash,
    pub header: Header,
}

type BlockBody = alloy_consensus::BlockBody<TransactionSigned, Header>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealedBlock {
    pub header: SealedHeader,
    pub body: BlockBody,
}

// ReadPrecompileCalls as newtype wrapper (matches nanoreth format)
pub type ReadPrecompileCall = (Address, Vec<(ReadPrecompileInput, ReadPrecompileResult)>);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ReadPrecompileCalls(pub Vec<ReadPrecompileCall>);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockAndReceipts {
    pub block: EvmBlock,
    pub receipts: Vec<LegacyReceipt>,
    #[serde(default)]
    pub system_txs: Vec<SystemTx>,
    #[serde(default)]
    pub read_precompile_calls: ReadPrecompileCalls,
    pub highest_precompile_address: Option<Address>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EvmBlock {
    Reth115(SealedBlock),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemTx {
    pub tx: Transaction,  // Custom Transaction, not reth_primitives::Transaction
    pub receipt: Option<LegacyReceipt>,
}
```

Key differences from the upstream evm-init types:
- `SealedBlock` is custom (not `reth_primitives::SealedBlock`)
- `TransactionSigned` has `signature` + custom `Transaction` enum
- `BlockAndReceipts` includes `highest_precompile_address` field
- `ReadPrecompileCalls` is a newtype wrapper struct
- `SystemTx.tx` uses the custom `Transaction` type

### CLI modifications for direct DB mode

Add to `Args` struct in `src/main.rs`:

```rust
#[derive(Parser)]
struct Args {
    /// Path to the abci state (not needed if --db-path and --block-number are set)
    file: Option<String>,

    #[arg(short, long, default_value = "hl-testnet-evm-blocks")]
    bucket: String,

    #[arg(short, long, default_value = "ap-northeast-1")]
    region: String,

    /// Direct path to EVM RocksDB (bypasses ABCI state deserialization)
    #[arg(long)]
    db_path: Option<String>,

    /// Block number (required with --db-path)
    #[arg(long)]
    block_number: Option<u64>,
}
```

And in `main()`, route to direct mode when both `--db-path` and `--block-number` are provided. Open RocksDB with `DB::open_for_read_only()` to avoid permission issues with root-owned Docker files.

---

## Step 4: Initialize the database

Run the command printed by evm-init:

```sh
reth-hl init-state --without-evm --chain mainnet \
  --header {block}.rlp \
  --header-hash 0x{hash} \
  {block}.jsonl --total-difficulty 0
```

Or with Docker:

```sh
docker run --rm \
  -v ~/nanoreth-mainnet:/root/.local/share/reth \
  -v /path/to/init-state:/init:ro \
  nanoreth:spot-429-fix \
  init-state --without-evm --chain mainnet \
  --header /init/{block}.rlp \
  --header-hash 0x{hash} \
  /init/{block}.jsonl --total-difficulty 0
```

**Expected output** (~4 minutes for 1.26M accounts):
```
Reth init-state starting
Setting up dummy EVM chain before importing state...
Initiating state dump
  parsed_new_accounts=285228
  ...
Writing accounts to db total_inserted_accounts=1263013
All accounts written to database, starting state root computation
Computed state root matches state root in state dump
Genesis block written hash=0x96fa9ea3...
```

> **Important**: The data directory **must be empty** before running init-state. If re-initializing:
> ```sh
> # Files are root-owned from Docker — use a container to delete
> docker run --rm -v ~/nanoreth-mainnet:/data alpine rm -rf /data/hyperliquid
> ```

---

## Step 5: Start nanoreth

```sh
# Docker (recommended)
docker run -d --name nanoreth-mainnet --network host \
  -v ~/nanoreth-mainnet:/root/.local/share/reth \
  -v ~/hl-mainnet/data/evm_block_and_receipts:/hl-blocks:ro \
  nanoreth:spot-429-fix node --chain mainnet \
  --local-ingest-dir /hl-blocks \
  --http --http.addr 127.0.0.1 --http.port 8547 --http.api eth,net,web3 \
  --ws --ws.addr 127.0.0.1 --ws.port 8548 --ws.api eth,net,web3

# Native
reth-hl node --chain mainnet \
  --local-ingest-dir ~/hl-mainnet/data/evm_block_and_receipts \
  --http --http.addr 127.0.0.1 --http.port 8547 --http.api eth,net,web3
```

The node will:
1. Download headers for the small gap between init-state and chain tip (~320 blocks if hl-node was running)
2. Download bodies, recover senders, execute — all 13 stages complete in under a minute
3. Start following the chain tip via hl-node's JSONL output

### Verify

```sh
curl -s http://127.0.0.1:8547 -X POST -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","method":"eth_blockNumber","id":1}' | python3 -c "
import sys, json
r = json.load(sys.stdin)['result']
print(f'Mainnet block: {int(r, 16):,}')
"
```

---

## Step 6: (Optional) Backfill historical blocks

After init-state, the node has state from the init block onward but **no historical blocks before that point**. Current-state queries work (`eth_call`, `eth_getBalance`, `eth_blockNumber`) but historical queries fail (`eth_getLogs` for old blocks, `eth_getBlockByNumber` for pre-init blocks).

To backfill:

```sh
# Download mainnet blocks from S3 (~28M blocks, estimated 100-150 GB)
aws s3 sync s3://hl-mainnet-evm-blocks/ ~/mainnet-blocks --request-payer requester
```

Then restart nanoreth with `--block-source`:

```sh
docker run -d --name nanoreth-mainnet --network host \
  -v ~/nanoreth-mainnet:/root/.local/share/reth \
  -v ~/mainnet-blocks:/blocks:ro \
  -v ~/hl-mainnet/data/evm_block_and_receipts:/hl-blocks:ro \
  nanoreth:spot-429-fix node --chain mainnet \
  --block-source /blocks \
  --local-ingest-dir /hl-blocks \
  --http --http.addr 127.0.0.1 --http.port 8547 --http.api eth,net,web3
```

> **Note**: Backfilling 28M blocks takes significant time and disk space. For many use cases (current state queries, following new events), running at tip without historical blocks is sufficient.

---

## Mainnet-Specific Troubleshooting

### Error: "http status: 429" panic in pseudo peer

**Cause**: `system_tx_to_reth_transaction()` calls `erc20_contract_to_spot_token(chain_id).unwrap()`. The Hyperliquid API returns HTTP 429 when rate-limited, and `.unwrap()` panics.

**Fix**: Use the `fix/spot-meta-429` branch which replaces the panic with graceful error handling and caches spot metadata to avoid repeated API calls.

### Error: "invalid type: byte array, expected any valid JSON value" on S3 block download

**Cause**: The `BlockAndReceipts` types in evm-init don't match the nanoreth serialization format. Specifically:
- evm-init uses `reth_primitives::SealedBlock` but blocks are serialized with nanoreth's custom `reth_compat::SealedBlock`
- Missing `highest_precompile_address` field
- `SystemTx.tx` uses wrong `Transaction` type

**Fix**: Replace types in evm-init's `src/types.rs` with the custom types shown in [Step 3](#type-definitions-for-evm-init).

### Error: "Permission denied" opening RocksDB

**Cause**: The EVM RocksDB files are owned by root (created by hl-node running inside Docker).

**Fix**: Open the database in read-only mode: `DB::open_for_read_only(&opts, db_path, false)` instead of `DB::open(&opts, db_path)`.

### Error: ABCI state deserialization fails

**Cause**: The `Exchange` struct in mainnet ABCI state has fields that `rmp_serde` can't skip (byte array types that fail `IgnoredAny` deserialization).

**Fix**: Use the `--db-path` + `--block-number` direct mode to bypass ABCI state deserialization entirely. Read the RocksDB directly and download the block header from S3.

### Pseudo peer disconnects after ~23 seconds (no init-state)

**Cause**: reth's P2P session has a ~23 second timeout. The headers stage downloads ~1M headers per session but doesn't checkpoint partway through. All downloaded headers are discarded on disconnect. 28M headers from genesis cannot be downloaded in time.

**Fix**: Use init-state. This is not optional for mainnet.

### Gossip port conflict (running both testnet and mainnet)

**Cause**: hl-node hardcodes gossip on ports 4001/4002 with no CLI option to change. Two hl-node instances on the same host will conflict.

**Fix**: Run one instance with host networking (gets native gossip) and the other with bridge networking (outbound gossip works, no inbound). The bridge-networked instance syncs fine but may be deprioritized by peers.

```yaml
# docker-compose.yml
hl-visor-testnet:
  network_mode: host  # native gossip on 4001/4002

hl-visor-mainnet:
  # default bridge network — no port conflict
  # outbound gossip works, inbound doesn't
```

### sysinfo crash on first container start

**Cause**: hl-visor panics when the `sysinfo` crate tries to read temperature sensors inside Docker containers that lack `/sys/class/thermal`.

**Fix**: Container restarts automatically (with `restart: unless-stopped`) and works on the second try. This is cosmetic — no data loss.

---

## Docker Compose (running both networks)

For running testnet + mainnet side by side, use a 4-container Docker Compose stack:

```yaml
services:
  hl-visor-testnet:
    build:
      context: .
      dockerfile: Dockerfile.hl-visor
      args:
        CHAIN: Testnet
    container_name: hl-visor-testnet
    network_mode: host
    volumes:
      - ./data/testnet:/root/hl
      - ./gossip/testnet.json:/root/override_gossip_config.json:ro
    restart: unless-stopped

  hl-visor-mainnet:
    build:
      context: .
      dockerfile: Dockerfile.hl-visor
      args:
        CHAIN: Mainnet
    container_name: hl-visor-mainnet
    volumes:
      - ./data/mainnet:/root/hl
      - ./gossip/mainnet.json:/root/override_gossip_config.json:ro
    restart: unless-stopped
    # Bridge network — avoids gossip port conflict with testnet

  nanoreth-testnet:
    image: nanoreth:latest
    container_name: nanoreth-testnet
    network_mode: host
    volumes:
      - ./nanoreth/testnet:/root/.local/share/reth
      - ./blocks/testnet:/blocks:ro
      - ./data/testnet/data/evm_block_and_receipts:/hl-blocks:ro
    command: >
      node --chain testnet
      --block-source /blocks
      --local-ingest-dir /hl-blocks
      --http --http.addr 127.0.0.1 --http.port 8545 --http.api eth,net,web3
      --ws --ws.addr 127.0.0.1 --ws.port 8546 --ws.api eth,net,web3
    depends_on:
      - hl-visor-testnet
    restart: unless-stopped

  nanoreth-mainnet:
    image: nanoreth:spot-429-fix
    container_name: nanoreth-mainnet
    network_mode: host
    volumes:
      - ./nanoreth/mainnet:/root/.local/share/reth
      - ./blocks/mainnet:/blocks:ro
      - ./data/mainnet/data/evm_block_and_receipts:/hl-blocks:ro
    command: >
      node --chain mainnet
      --block-source /blocks
      --local-ingest-dir /hl-blocks
      --http --http.addr 127.0.0.1 --http.port 8547 --http.api eth,net,web3
      --ws --ws.addr 127.0.0.1 --ws.port 8548 --ws.api eth,net,web3
    depends_on:
      - hl-visor-mainnet
    restart: unless-stopped
```

### Port allocation

| Service | HTTP RPC | WebSocket | Auth RPC | Gossip |
|---------|----------|-----------|----------|--------|
| Testnet | 8545 | 8546 | 8551 | 4001/4002 (host) |
| Mainnet | 8547 | 8548 | 8552 | 4001/4002 (bridge) |

### Dockerfile.hl-visor

```dockerfile
FROM ubuntu:24.04
RUN apt-get update && apt-get install -y curl ca-certificates gpg && rm -rf /var/lib/apt/lists/*

ARG CHAIN=Testnet
ENV CHAIN=${CHAIN}

# Import Hyperliquid GPG key
RUN curl -sL https://raw.githubusercontent.com/hyperliquid-dex/node/main/pub_key.asc | gpg --import

# Download hl-visor binary
RUN if [ "$CHAIN" = "Mainnet" ]; then \
      curl -sL https://binaries.hyperliquid.xyz/Mainnet/hl-visor -o /usr/local/bin/hl-visor; \
    else \
      curl -sL https://binaries.hyperliquid-testnet.xyz/Testnet/hl-visor -o /usr/local/bin/hl-visor; \
    fi && chmod +x /usr/local/bin/hl-visor

# visor.json must be next to the binary
RUN echo "{\"chain\": \"${CHAIN}\"}" > /usr/local/bin/visor.json

WORKDIR /root
VOLUME ["/root/hl"]

ENTRYPOINT ["hl-visor", "run-non-validator", "--serve-evm-rpc", "--disable-output-file-buffering"]
```

> **Gotcha**: `visor.json` must be in the same directory as the `hl-visor` binary (`/usr/local/bin/visor.json`), not in `$HOME`. Also needs `gpg` installed for binary signature verification.
