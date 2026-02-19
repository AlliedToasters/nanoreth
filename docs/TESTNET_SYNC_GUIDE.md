# HyperEVM Testnet Sync Guide

Complete guide to syncing a nanoreth testnet archive node from scratch, including all workarounds, pitfalls, and lessons learned from a full sync to block 46M+.

## Table of Contents

- [Architecture Overview](#architecture-overview)
- [Prerequisites](#prerequisites)
- [Step 1: Build nanoreth](#step-1-build-nanoreth)
- [Step 2: Get testnet genesis](#step-2-get-testnet-genesis)
- [Step 3: Initialize the database](#step-3-initialize-the-database)
- [Step 4: Download blocks](#step-4-download-blocks)
- [Step 5: Start the node](#step-5-start-the-node)
- [Step 6: Monitor sync progress](#step-6-monitor-sync-progress)
- [Troubleshooting](#troubleshooting)
- [Operational Notes](#operational-notes)

---

## Architecture Overview

Nanoreth is **not** a standard Ethereum node. Understanding how it differs from reth is critical for debugging:

- **Standard reth**: Downloads headers/bodies from P2P peers, re-executes every transaction to produce receipts and state.
- **Nanoreth**: Blocks arrive pre-executed from HyperLiquid's L1 validators. Receipts are transferred alongside blocks (not recomputed). A "pseudo peer" process serves blocks from an external source (S3, local files, or another nanoreth node) to the main reth engine via localhost P2P.

Key implications:
- `--bootnodes` and `--trusted-peers` will **not** trigger historical sync. You need `--block-source`.
- Blocks include HyperLiquid-specific fields: `system_tx_count`, `read_precompile_calls`, `highest_precompile_address`.
- The chain is **post-merge from block 0** (Paris TTD=0). All hardforks through Cancun are active at genesis.
- Chain ID is **998** (testnet).

### Sync pipeline stages

The pipeline runs 13 stages sequentially. If any stage fails, it unwinds all stages back to the last committed checkpoint and retries from there:

```
Era -> Headers -> Bodies -> SenderRecovery -> Execution ->
AccountHashing -> StorageHashing -> MerkleExecute ->
TransactionLookup -> IndexStorageHistory -> IndexAccountHistory -> Finish
```

**Execution** is where most problems occur — it re-runs EVM transactions and validates gas usage against the block header. Any data quality issue in the block files will surface here.

---

## Prerequisites

| Requirement | Notes |
|-------------|-------|
| Rust & Cargo | `curl https://sh.rustup.rs -sSf \| sh` |
| Docker (optional) | For containerized deployment |
| AWS CLI | For S3 block downloads (`aws s3 sync`) |
| AWS credentials | Requester-pays access to `s3://hl-testnet-evm-blocks` |
| Python 3.8+ | For gap-filling scripts |
| ~200 GB disk | ~170 GB for blocks, ~16 GB for reth DB, headroom for growth |

---

## Step 1: Build nanoreth

### Option A: Native build

```sh
git clone https://github.com/AlliedToasters/nanoreth.git
cd nanoreth
make install
```

This installs the `reth-hl` binary.

### Option B: Docker build

```sh
git clone https://github.com/AlliedToasters/nanoreth.git
cd nanoreth
docker build -t nanoreth .
```

Build takes 15-30 minutes (Rust release compilation).

> **Gotcha**: If you modify source code, Docker's layer cache only helps with dependency compilation. The final `reth_hl` crate recompiles in ~60 seconds.

---

## Step 2: Get testnet genesis

```sh
cd ~
git clone https://github.com/sprites0/hl-testnet-genesis
zstd --rm -d ~/hl-testnet-genesis/*.zst
```

This provides state snapshots at various block heights. Use the latest available (block 34,112,653).

> **Gotcha**: The repository contains both `.rlp` and `.jsonl` files at each block height. The format you need depends on which `init-state` path you use (see next step).

---

## Step 3: Initialize the database

This seeds reth's database with the account/storage state at block 34,112,653. The node will sync forward from there.

### Recommended method (current upstream)

```sh
reth-hl init-state --without-evm --chain testnet \
  --header ~/hl-testnet-genesis/34112653.rlp \
  --header-hash 0xeb79aca618ab9fda6d463fddd3ad439045deada1f539cbab1c62d7e6a0f5859a \
  ~/hl-testnet-genesis/34112653.jsonl --total-difficulty 0
```

Or with Docker:

```sh
docker run --rm \
  -v ~/.nanoreth-data:/root/.local/share/reth \
  -v ~/hl-testnet-genesis:/genesis:ro \
  nanoreth init-state --without-evm --chain testnet \
  --header /genesis/34112653.rlp \
  --header-hash 0xeb79aca618ab9fda6d463fddd3ad439045deada1f539cbab1c62d7e6a0f5859a \
  /genesis/34112653.jsonl --total-difficulty 0
```

### Pitfalls

| Problem | Cause | Fix |
|---------|-------|-----|
| "Is a directory" error | Passed a directory instead of a file path | Make sure the JSONL path points to the file, not its parent directory (fix #2) |
| "IntegerList must be pre-sorted and non-empty" | Using wrong combination of image version and genesis file format | Use the current upstream image with `.rlp` + `.jsonl` as shown above |
| DB not empty error | Running init-state on an existing database | Delete `~/.nanoreth-data/` (or your data dir) first |

> **Important**: The data directory **must be empty** before running init-state. If you need to re-initialize, delete the entire directory first:
> ```sh
> rm -rf ~/.nanoreth-data/998/  # or wherever your data lives
> ```

---

## Step 4: Download blocks

This is the most time-consuming step and the most error-prone. The block data lives in S3 as MessagePack + LZ4 compressed files, organized in a height-1 bucketed directory structure:

```
{million}/{thousand}/{block_number}.rmp.lz4
# Example: 45000000/45895000/45895963.rmp.lz4
```

### 4a. Sync from S3 (primary source)

```sh
aws s3 sync s3://hl-testnet-evm-blocks/ ~/evm-blocks --request-payer requester
```

This downloads ~170 GB. Takes several hours depending on bandwidth. You can run the node before the download completes — it will sync as far as blocks are available.

### 4b. Fix boundary block gaps

An S3 bucketing bug causes blocks at exact multiples of 1000 to be placed in the wrong directory. Download them separately:

```sh
pip install -r scripts/requirements.txt
python scripts/download_boundary_blocks.py --blocks-dir ~/evm-blocks
```

### 4c. Verify completeness

```sh
python scripts/check_block_completeness.py --blocks-dir ~/evm-blocks --fix
```

The `--fix` flag re-downloads any blocks still missing from S3.

### 4d. Fill remaining gaps from public RPC

S3 has a few hundred genuine gaps. Fill them from the public testnet RPC:

```sh
python scripts/fetch_blocks_rpc.py --blocks-dir ~/evm-blocks
```

### Pitfalls and critical ordering

**Always sync from S3 BEFORE using the RPC script.** This is the single most important operational rule. Here's why:

1. **S3 blocks are canonical.** They include `read_precompile_calls` data and correctly encoded transactions.
2. **RPC-fetched blocks are lossy.** They are missing `read_precompile_calls` (always `[]`) and may encode Legacy transaction signatures differently (full EIP-155 `v` instead of normalized `yParity`, missing `chainId` field).
3. **RPC blocks can overwrite S3 blocks.** If you run `fetch_blocks_rpc.py` for a range and then `aws s3 sync`, the sync won't detect the difference because it uses modification time, not content comparison.

**To detect and replace RPC-fetched blocks that S3 now covers:**

```sh
aws s3 sync s3://hl-testnet-evm-blocks/ ~/evm-blocks --request-payer requester --size-only
```

The `--size-only` flag compares file sizes rather than timestamps. RPC-fetched blocks are slightly different sizes than S3 originals (e.g. 592 vs 569 bytes for empty blocks) so this reliably detects and replaces them.

> **Gotcha**: `check_block_completeness.py` checks whether files exist, not whether they're S3-sourced or RPC-sourced. A "complete" cache can still have RPC-fetched blocks that cause execution failures.

---

## Step 5: Start the node

### Native

```sh
reth-hl node --chain testnet \
  --block-source ~/evm-blocks \
  --http --http.addr 0.0.0.0 --http.api eth,net,web3 \
  --http.port 8545
```

### Docker

```sh
docker run -d --name nanoreth-testnet --network host \
  -v ~/.nanoreth-data:/root/.local/share/reth \
  -v ~/evm-blocks:/blocks:ro \
  nanoreth node --chain testnet --block-source /blocks \
  --http --http.addr 0.0.0.0 --http.port 8545 --http.api eth,net,web3
```

### Alternative: S3 direct (no local block cache)

```sh
docker run -d --name nanoreth-testnet --network host \
  -v ~/.nanoreth-data:/root/.local/share/reth \
  -v ~/.aws:/root/.aws:ro \
  nanoreth node --chain testnet --block-source s3://hl-testnet-evm-blocks \
  --http --http.addr 0.0.0.0 --http.port 8545 --http.api eth,net,web3
```

### Alternative: Sync from another nanoreth node

If you have access to a synced node (with `--enable-sync-server`):

```sh
reth-hl node --chain testnet \
  --block-source rpc://synced-node:8545 \
  --http --http.api eth,net,web3
```

This is the best option for complete data — the `rpc://` source includes `read_precompile_calls`, correct transaction encoding, and everything else.

---

## Step 6: Monitor sync progress

### Check pipeline stages

```sh
curl -s -X POST http://127.0.0.1:8545 \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","method":"eth_syncing","id":1}' | python3 -m json.tool
```

Each stage reports its checkpoint block. The pipeline is fully synced when all stages reach the same target block and the `Finish` stage is at the target.

### Check latest block

```sh
curl -s -X POST http://127.0.0.1:8545 \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","method":"eth_blockNumber","id":1}' | python3 -c "
import sys, json
r = json.load(sys.stdin)['result']
print(f'Latest block: {int(r, 16):,}')
"
```

### Watch logs for errors

```sh
docker logs -f nanoreth-testnet 2>&1 | grep -E "ERROR|WARN|bad_block|gas.used.mismatch|nonce"
```

### Expected sync timeline

| Stage | Blocks 34M-46M | Notes |
|-------|----------------|-------|
| Headers | ~20 seconds | Downloads headers from pseudo peer |
| Bodies | ~5 seconds | Very fast with hash cache fix |
| SenderRecovery | ~1 second | Recovers tx sender addresses |
| Execution | ~7 seconds | Re-runs EVM, validates gas. **Most likely to fail.** |
| AccountHashing | ~2 seconds | Hashes account trie |
| StorageHashing | ~7 seconds | Hashes storage trie |
| MerkleExecute | ~4 seconds | Merkle tree computation |
| TransactionLookup | ~1 second | Indexes tx hashes |
| IndexStorageHistory | ~8 seconds | Historical storage index |
| IndexAccountHistory | ~2 seconds | Historical account index |
| **Total** | **~1 minute** | For 12M blocks (34M to 46M) |

---

## Troubleshooting

### Error: "Post-merge network, but never seen beacon client"

**Cause**: The pipeline doesn't start because `--block-source /local-path` doesn't generate forkchoice updates.

**Fix**: Use this fork. Fix #1 (local block source pipeline trigger) sends a direct `fork_choice_updated()` when blocks are available. Upstream `node-builder` branch may not include this yet.

### Error: "nonce N too high, expected 0"

**Cause**: Legacy transactions with EIP-155 encoded `v` values (e.g. 2032) but missing `chainId` field. Reth recovers the wrong sender address, which has never transacted (nonce 0).

**Fix**: Use this fork. Fix #5 (EIP-155 chain_id extraction) extracts `chain_id = (v - 35) / 2` from the signature during deserialization. Verify with:
```sh
# Check the bad block locally — if chain_id is None for Legacy txs, the fix isn't applied
python3 -c "
import lz4.frame, msgpack, os
bn = 45895963  # replace with bad block number
m = (bn//1000000)*1000000; t = (bn//1000)*1000
path = os.path.expanduser(f'~/evm-blocks/{m}/{t}/{bn}.rmp.lz4')
with open(path,'rb') as f: data = lz4.frame.decompress(f.read())
block = msgpack.unpackb(data, raw=False)
print(block[0].keys())
"
```

### Error: "gas used mismatch: got X, expected Y"

**Cause**: Almost always an RPC-fetched block with missing `read_precompile_calls`. The transaction calls an HL L1 precompile (0x0800-0x0813), but without the recorded response, nanoreth's dummy precompile returns `OutOfGas`, causing a different execution path and different gas usage.

**Diagnosis**:
```python
# Check if the bad block has empty precompile data
python3 -c "
import lz4.frame, msgpack, os
bn = 45896781  # replace with bad block number
m = (bn//1000000)*1000000; t = (bn//1000)*1000
path = os.path.expanduser(f'~/evm-blocks/{m}/{t}/{bn}.rmp.lz4')
with open(path,'rb') as f: data = lz4.frame.decompress(f.read())
block = msgpack.unpackb(data, raw=False)
rpc = block[0]['read_precompile_calls']
txs = block[0]['block'][1].get('transactions', [])
print(f'Transactions: {len(txs)}')
print(f'Precompile calls: {\"EMPTY\" if not rpc else \"present\"} ({len(rpc)} entries)')
"
```

**Fix**: Re-sync the block from S3:
```sh
# Determine the thousand-block directory
# For block 45,896,781: million=45000000, thousand=45896000
aws s3 sync s3://hl-testnet-evm-blocks/45000000/45896000/ \
  ~/evm-blocks/45000000/45896000/ --request-payer requester
```

If the block is genuinely missing from S3, see [Blocks missing from S3](#blocks-missing-from-s3).

### Error: Pseudo peer disconnects every ~45 seconds during Bodies stage

**Cause**: `GetBlockBodies` requests are by hash, but the hash-to-block-number cache wasn't populated during the `GetBlockHeaders` phase. Cache misses trigger sequential backfill scans that block the single-threaded event loop for minutes, causing protocol timeout disconnects.

**Fix**: Use this fork. Fix #4 (pseudo peer hash cache) caches hash-to-number mappings during header serving and increases the LRU cache to 15M entries.

### Error: Invalid `yParity` value during deserialization

**Cause**: Some testnet blocks encode `yParity` as 27/28 (legacy Ethereum convention) instead of 0/1.

**Fix**: Use this fork. Fix #3 (yParity normalization) handles both conventions during deserialization.

### Pipeline stuck in unwind loop

**Symptom**: Logs show the pipeline downloading headers, running Bodies, then hitting an error at the same block, unwinding, and repeating.

**Diagnosis**:
```sh
docker logs nanoreth-testnet 2>&1 | grep -E "ERROR|bad_block" | tail -5
```

**Fix**: The error message tells you which block failed and why. Follow the specific error's troubleshooting section above. After fixing (e.g. replacing an RPC block with S3 version), restart the container.

---

## Operational Notes

### Blocks missing from S3

The S3 archive has a few hundred genuine gaps. For these blocks, `fetch_blocks_rpc.py` is the only option, but RPC-fetched blocks lack precompile data. Strategies:

1. **Most RPC-only blocks are fine.** Only ~1% of transaction-bearing blocks call precompiles. Most will execute correctly.
2. **If a specific block fails**, check if S3 has added it since your last sync. S3 coverage improves over time.
3. **If you have access to a synced nanoreth node**, use `eth_blockPrecompileData` to backfill:
   ```sh
   curl -X POST http://synced-node:8545 \
     -H "Content-Type: application/json" \
     -d '{"jsonrpc":"2.0","method":"eth_blockPrecompileData","params":["0xBLOCK_HEX"],"id":1}'
   ```
4. **Report S3 gaps upstream** at [hl-archive-node/nanoreth](https://github.com/hl-archive-node/nanoreth/issues).

### Database management

```sh
# Check database size
du -sh ~/.nanoreth-data/

# Unwind all stages to a specific block (destructive — resets progress)
reth-hl stage unwind --chain testnet to-block 45612676

# Run a single stage manually
reth-hl stage run --chain testnet --from 45612676 --to 46041000 --commit --skip-unwind execution
```

### Keeping the node up to date

The node syncs to whatever blocks are available in the block source directory. To advance:

```sh
# Sync latest blocks from S3
aws s3 sync s3://hl-testnet-evm-blocks/ ~/evm-blocks --request-payer requester

# Fill tip from RPC (for blocks not yet on S3)
python scripts/fetch_blocks_rpc.py --blocks-dir ~/evm-blocks

# Restart the node to pick up new blocks
docker restart nanoreth-testnet
```

### System transactions

HyperEVM includes system transactions from addresses like `0x222...22` / `0x200..xx`. These have `gasPrice = 0` and are handled specially:
- Their gas is **not counted** toward the block's `gasUsed`.
- They appear in the block if the node runs without `--hl-node-compliant`.
- The `--hl-node-compliant` flag hides system txs and their receipts.

### HL precompiles (0x0800-0x0813)

HyperEVM has custom precompile addresses that bridge to the L1. During block execution, nanoreth replays recorded precompile call/response pairs from `read_precompile_calls`. If a precompile address is called but has no recording, a dummy precompile returns `OutOfGas`.

The `highest_precompile_address` field tracks which precompiles exist at each block height:
- Block 0+: up to `0x0810`
- Block 41,121,887+: up to `0x0811`
- Block 42,675,776+: up to `0x0812`
- Block 44,868,476+: up to `0x0813`

### Data directory layout

```
~/.nanoreth-data/998/          # Chain ID 998 (testnet)
  db/                          # RocksDB — accounts, storage, receipts (~6 GB)
  static_files/                # Segmented headers/bodies/receipts (~10 GB)
  reth.toml                    # Node configuration
  jwt.hex                      # Engine API JWT
  discovery-secret             # P2P identity
  known-peers.json             # Peer cache
```

### Docker image tags

When building Docker images, tag them descriptively:
```sh
docker build -t nanoreth:chain-id-fix .      # After the chain_id fix
docker build -t nanoreth:$(git rev-parse --short HEAD) .  # By git commit
```

---

## Fork Fixes Summary

This fork includes 5 fixes on top of upstream `node-builder`. All are required for a successful testnet sync with local block sources:

| # | Fix | File | Error without it |
|---|-----|------|-----------------|
| 1 | Local block source pipeline trigger | `src/node/network/mod.rs` | "Post-merge network, but never seen beacon client" — pipeline never starts |
| 2 | Init-state path validation | `src/cli/init_state.rs` | "Is a directory" OS error on init |
| 3 | yParity normalization | `src/node/types/block.rs` | Deserialization panic on blocks with legacy 27/28 parity values |
| 4 | Pseudo peer hash cache | `src/pseudo_peer/service.rs` | Bodies stage disconnects every ~45s, effectively stuck |
| 5 | EIP-155 chain_id extraction | `src/node/types/reth_compat.rs` | "nonce N too high, expected 0" on blocks with missing chainId |
