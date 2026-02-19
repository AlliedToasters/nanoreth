# nanoreth (testnet fork)

Fork of [hl-archive-node/nanoreth](https://github.com/hl-archive-node/nanoreth) focused on running a **HyperEVM testnet** archive node for NFT sales indexing.

Upstream nanoreth is a HyperEVM archive node implementation based on [reth](https://github.com/paradigmxyz/reth). See the [upstream README](https://github.com/hl-archive-node/nanoreth) for general documentation.

## Fork changes

This fork adds fixes needed for testnet sync with local block sources:

1. **Local block source pipeline trigger** ([PR #118](https://github.com/hl-archive-node/nanoreth/pull/118)): `--block-source /local-path` now triggers the sync pipeline via a direct `fork_choice_updated()` call. Without this, the pipeline never starts because the pseudo peer's block announcements don't generate forkchoice updates on this post-merge chain.

2. **Init-state path validation**: Prevents a cryptic "Is a directory" error when `init-state` is given a directory instead of a file.

3. **yParity normalization**: Normalizes legacy `yParity` values (27/28) to 0/1 during S3 block deserialization. Some older testnet blocks use pre-EIP-155 parity encoding, which causes deserialization failures without this fix.

4. **Pseudo peer hash cache fix**: Caches block hash-to-number mappings during `GetBlockHeaders` responses and increases the LRU cache from 1M to 15M entries. Without this, the `GetBlockBodies` handler (which receives requests by hash) triggers slow backfill scans that block the pseudo peer event loop, causing protocol timeout disconnects. Also removes the public RPC fallback in favor of hard failure for faster debugging.

5. **EIP-155 chain\_id extraction**: Extracts `chain_id` from Legacy transaction signature `v` values during msgpack deserialization. Some block sources (notably RPC-fetched blocks) omit the `chainId` field from Legacy transactions while encoding the chain\_id in the EIP-155 `v` value (e.g. `v=2032` for chain\_id=998). Without extraction, reth computes the wrong transaction hash and recovers the wrong sender address, causing "nonce too high" execution errors.

### Branch structure

- **`main`**: All fixes on top of upstream `node-builder` — use this to run the testnet node
- **`fix/*` branches**: Isolated single-commit branches for clean upstream PRs

## Upstream

## ⚠️ IMPORTANT: System Transactions Appear as Pseudo Transactions

Deposit transactions from [System Addresses](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/hyperevm/hypercore-less-than-greater-than-hyperevm-transfers#system-addresses) like `0x222..22` / `0x200..xx` to user addresses are intentionally recorded as pseudo transactions.
This change simplifies block explorers, making it easier to track deposit timestamps.
Ensure careful handling when indexing.

To disable this behavior, add --hl-node-compliant to the CLI arguments-this will not show system transactions and their receipts, mimicking hl-node's output.

## Prerequisites

Building nanoreth from source requires Rust and Cargo to be installed:

`$ curl https://sh.rustup.rs -sSf | sh`

## How to run (mainnet)

1) Setup AWS credentials at `~/.aws/credentials` with read S3 access.

2) `$ make install` - this will install the reth-hl binary.

3) Start nanoreth which will begin syncing using the blocks in `~/evm-blocks`:

```sh
reth-hl node --http --http.addr 0.0.0.0 --http.api eth,ots,net,web3 \
  --ws --ws.addr 0.0.0.0 --ws.origins '*' --ws.api eth,ots,net,web3 \
  --ws.port 8545 --http.port 8545 --s3
```

## How to run (mainnet) (with local block sync)

The `--s3` method above fetches blocks from S3, but you can instead source them from your local hl-node using `--ingest-dir` and `--local-ingest-dir`.

This will require you to first have a hl-node outputting blocks prior to running the initial s3 sync,
the node will prioritise locally produced blocks with a fallback to s3.
This method will allow you to reduce the need to rely on S3.

This setup adds `--local-ingest-dir=<path>` (or a shortcut: `--local` if using default hl-node path) to ingest blocks from hl-node, and `--ingest-dir` for fallback copy of EVM blocks. `--ingest-dir` can be replaced with `--s3` if you don't want to
periodically run `aws s3 sync` as below.

```sh
# Run your local hl-node (make sure output file buffering is disabled)
# Make sure evm blocks are being produced inside evm_block_and_receipts
$ hl-node run-non-validator --replica-cmds-style recent-actions --serve-eth-rpc --disable-output-file-buffering

# Fetch EVM blocks (Initial sync)
$ aws s3 sync s3://hl-mainnet-evm-blocks/ ~/evm-blocks --request-payer requester # one-time

# Run node (with local-ingest-dir arg)
$ make install
$ reth-hl node --http --http.addr 0.0.0.0 --http.api eth,ots,net,web3 \
    --ws --ws.addr 0.0.0.0 --ws.origins '*' --ws.api eth,ots,net,web3 --ingest-dir ~/evm-blocks --local-ingest-dir <path-to-your-hl-node-evm-blocks-dir> --ws.port 8545
```

## How to run (syncing from another nanoreth node via RPC)

If you already have a nanoreth node running (e.g. in the cloud with S3 access), you can sync a local node from it without needing S3 credentials.

**On the serving node** (cloud), add `--enable-sync-server` to its launch flags:

```sh
reth-hl node --http --http.addr 0.0.0.0 --http.api eth,ots,net,web3 \
  --ws --ws.addr 0.0.0.0 --ws.origins '*' --ws.api eth,ots,net,web3 \
  --ws.port 8545 --http.port 8545 --s3 --enable-sync-server
```

**On the local node**, point `--block-source` to the serving node's RPC:

```sh
reth-hl node --http --http.api eth,ots,net,web3 \
  --block-source=rpc://your-cloud-node:8545
```

The `--rpc.polling-interval` flag controls how often the local node polls for new blocks (default: 100ms).

## Architecture: How nanoreth differs from reth

Nanoreth replaces reth's native P2P sync pipeline with a **pseudo peer + block source** architecture:

- **Standard reth** syncs by downloading headers and bodies from P2P peers, then re-executing every transaction locally to produce receipts and state. The eth wire protocol only transfers headers and bodies — receipts are never sent over P2P because each node generates its own.
- **Nanoreth** does not re-execute blocks. Blocks arrive pre-executed from hl-node (the Go Hyperliquid node), so receipts must be **transferred** alongside blocks. Since the eth wire protocol has no mechanism for this, nanoreth fetches complete blocks (with receipts) from an external **block source** (S3, local files, or another nanoreth node via RPC). A "pseudo peer" process connects to the main node as a localhost P2P peer and serves these blocks when requested. Blocks are imported via the engine API (`new_payload` + `fork_choice_updated`), bypassing reth's download pipeline entirely.

This means reth's `--bootnodes` and `--trusted-peers` flags will establish P2P connections but **will not trigger historical block sync** — the sync pipeline stages that request blocks from peers are not active in nanoreth. A block source (`--s3`, `--local`, `--block-source`) is required for syncing.

Nanoreth also extends reth's block types with Hyperliquid-specific fields (`system_tx_count`, `read_precompile_calls`, `highest_precompile_address`, blob `sidecars`) that are not part of the standard Ethereum wire protocol, further requiring the custom sync path.

## How to run (testnet)

Testnet is supported since block 34,112,653. This fork includes fixes needed for testnet sync with local block sources (see [Fork changes](#fork-changes)).

### 1. Get testnet genesis

```sh
cd ~
git clone https://github.com/sprites0/hl-testnet-genesis
zstd --rm -d ~/hl-testnet-genesis/*.zst
```

### 2. Initialize the database

```sh
make install
reth-hl init-state --without-evm --chain testnet --header ~/hl-testnet-genesis/34112653.rlp \
  --header-hash 0xeb79aca618ab9fda6d463fddd3ad439045deada1f539cbab1c62d7e6a0f5859a \
  ~/hl-testnet-genesis/34112653.jsonl --total-difficulty 0
```

### 3. Download blocks

Download testnet blocks from S3 to a local cache (requires AWS credentials with requester-pays access):

```sh
aws s3 sync s3://hl-testnet-evm-blocks/ ~/evm-blocks --request-payer requester
```

Then fix known gaps in the S3 data:

```sh
pip install -r scripts/requirements.txt

# Download boundary blocks (multiples of 1000) missed by an S3 bucketing bug
python scripts/download_boundary_blocks.py --blocks-dir ~/evm-blocks

# Verify completeness and re-download any remaining gaps from S3
python scripts/check_block_completeness.py --blocks-dir ~/evm-blocks --fix

# Fill from the local cache tip to the chain tip via public RPC (rate-limited)
python scripts/fetch_blocks_rpc.py --blocks-dir ~/evm-blocks
```

### 4. Run the node

```sh
reth-hl node --chain testnet \
  --block-source ~/evm-blocks \
  --http --http.addr 0.0.0.0 --http.api eth,net,web3 \
  --http.port 8545
```

### Docker

```sh
# Build the image
docker build -t nanoreth .

# Initialize (one-time)
docker run --rm \
  -v ~/.nanoreth-data:/root/.local/share/reth \
  -v ~/hl-testnet-genesis:/genesis:ro \
  nanoreth init-state --without-evm --chain testnet \
  --header /genesis/34112653.rlp \
  --header-hash 0xeb79aca618ab9fda6d463fddd3ad439045deada1f539cbab1c62d7e6a0f5859a \
  /genesis/34112653.jsonl --total-difficulty 0

# Run
docker run -d --name nanoreth-testnet --network host \
  -v ~/.nanoreth-data:/root/.local/share/reth \
  -v ~/evm-blocks:/blocks:ro \
  nanoreth node --chain testnet --block-source /blocks \
  --http --http.addr 0.0.0.0 --http.port 8545 --http.api eth,net,web3
```

### Monitoring sync progress

```sh
curl -s -X POST http://127.0.0.1:8545 \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","method":"eth_syncing","id":1}' | python3 -m json.tool
```

The pipeline processes stages sequentially: Headers, Bodies, Execution, SenderRecovery, AccountHashing, StorageHashing, Merkle, TransactionLookup, IndexAccountHistory, IndexStorageHistory, Finish.

## Scripts

The S3 block archive has gaps and boundary-block bucketing bugs. These Python scripts let operators audit their local block cache, fill gaps from S3, fetch missing blocks from the public RPC, and fix known bucketing issues — all without running the node itself.

Install dependencies:

```sh
pip install -r scripts/requirements.txt
```

### check_block_completeness.py

Scans the local block cache and reports missing blocks by comparing against the expected (height-1)-based directory layout.

```sh
# Full scan (auto-detects latest block)
python scripts/check_block_completeness.py --blocks-dir /path/to/blocks

# Scan a specific range
python scripts/check_block_completeness.py --blocks-dir /path/to/blocks --start 45000000 --end 45010000

# List every missing block
python scripts/check_block_completeness.py --blocks-dir /path/to/blocks --verbose

# Re-download missing blocks from S3
python scripts/check_block_completeness.py --blocks-dir /path/to/blocks --fix
```

### fetch_blocks_rpc.py

Fetches missing blocks from the public Hyperliquid testnet RPC and writes them in nanoreth's MessagePack + LZ4 format (Reth115 structure). Useful for filling gaps near the chain tip that aren't yet on S3. Rate-limited to ~120 calls/min.

**Important limitation:** RPC-fetched blocks are missing `read_precompile_calls` data (always set to `[]`) because the public RPC does not expose precompile call recordings. Most blocks don't call precompiles, but the ~1% that do will fail execution with a gas mismatch error. See [Known issue: RPC-fetched blocks missing precompile data](#known-issue-rpc-fetched-blocks-missing-precompile-data) for details and workarounds.

```sh
# Fill from cache latest to chain tip
python scripts/fetch_blocks_rpc.py --blocks-dir /path/to/blocks

# Fill a specific range
python scripts/fetch_blocks_rpc.py --blocks-dir /path/to/blocks --start 45895888 --end 46000000

# Dry run
python scripts/fetch_blocks_rpc.py --blocks-dir /path/to/blocks --dry-run
```

### download_boundary_blocks.py

Downloads missing boundary blocks (multiples of 1000) from S3. These blocks were skipped by an earlier bucketing bug and need to be fetched separately.

```sh
# Download all missing boundary blocks
python scripts/download_boundary_blocks.py --blocks-dir /path/to/blocks

# Dry run (count only)
python scripts/download_boundary_blocks.py --blocks-dir /path/to/blocks --dry-run
```

## Known issue: RPC-fetched blocks missing precompile data

### Problem

The S3 testnet block archive has a few hundred gaps where block files are missing. The `fetch_blocks_rpc.py` script fills these gaps by fetching blocks from the public Hyperliquid testnet RPC. However, RPC-fetched blocks differ from S3-sourced blocks in two ways:

1. **Missing `read_precompile_calls`**: Always set to `[]`. The public RPC does not expose precompile call recordings. When the Execution stage encounters a block whose transaction internally calls an HL L1 precompile (addresses `0x0800`-`0x0813`), nanoreth registers a dummy precompile that returns `OutOfGas`. This causes the contract to take a different code path, producing a gas mismatch (e.g. "got 1881520, expected 139990") and incorrect state transitions. The pipeline unwinds and gets stuck in a loop.

2. **Legacy transaction encoding differences**: The RPC returns the full EIP-155 `v` value (e.g. 2032) in the signature but may omit the `chainId` field from Legacy transaction bodies. Fix #5 (EIP-155 chain\_id extraction) handles this at deserialization time.

Only ~1% of transaction-bearing blocks actually invoke precompiles, so most RPC-fetched blocks execute correctly. The issue only manifests when a transaction in an RPC-fetched block calls an `0x08xx` precompile.

### Mitigation strategy

In priority order:

1. **Re-sync from S3 first.** Always run `aws s3 sync` before `fetch_blocks_rpc.py` so that S3-available blocks (which include precompile data) are not overwritten by RPC-fetched versions. Use `--size-only` to detect and replace any RPC-fetched blocks that S3 now covers:
   ```sh
   aws s3 sync s3://hl-testnet-evm-blocks/ ~/evm-blocks --request-payer requester --size-only
   ```

2. **Sync from another nanoreth node.** If you have access to a synced nanoreth node (yours or someone else's), use the `rpc://` block source or the `eth_blockPrecompileData` RPC endpoint to get complete block data including precompile recordings:
   ```sh
   # Option A: Use rpc:// source for full sync (includes precompile data)
   reth-hl node --chain testnet --block-source rpc://synced-node:8545 ...

   # Option B: Query precompile data for a specific block
   curl -X POST http://synced-node:8545 \
     -H "Content-Type: application/json" \
     -d '{"jsonrpc":"2.0","method":"eth_blockPrecompileData","params":["0x2BC544D"],"id":1}'
   ```

3. **Identify affected blocks.** Most RPC-fetched blocks don't call precompiles and execute fine. To find which ones do, run the Execution stage and note the `bad_block` number from any gas mismatch error. Then check if that block's `read_precompile_calls` is empty and whether it has transactions that interact with HL precompiles.

4. **Report S3 gaps upstream.** File an issue at [hl-archive-node/nanoreth](https://github.com/hl-archive-node/nanoreth/issues) or ask in the [Hyperliquid Discord](https://discord.gg/hyperliquid) #node-operators channel to get the missing blocks added to the S3 archive.
