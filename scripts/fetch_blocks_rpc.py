#!/usr/bin/env python3
"""
Fetch missing blocks from public RPC and write them in nanoreth format.

Scans the local block cache for gaps (relative to the chain tip) and fills them
by fetching block data + receipts from the public Hyperliquid RPC.

Output format: MessagePack + LZ4 compressed, matching nanoreth's Reth115 structure.

Usage:
    # Fill tip gap (testnet, default)
    python scripts/fetch_blocks_rpc.py --blocks-dir /path/to/blocks

    # Fill tip gap (mainnet)
    python scripts/fetch_blocks_rpc.py --blocks-dir /path/to/blocks --chain mainnet

    # Fill a specific range
    python scripts/fetch_blocks_rpc.py --blocks-dir /path/to/blocks --start 45895888 --end 46000000

    # Custom RPC endpoint
    python scripts/fetch_blocks_rpc.py --blocks-dir /path/to/blocks --rpc https://rpc.hyperliquid-testnet.xyz/evm

    # Dry run (show what would be fetched)
    python scripts/fetch_blocks_rpc.py --blocks-dir /path/to/blocks --dry-run
"""

import argparse
import json
import logging
import os
import sys
import time
from pathlib import Path

import lz4.frame
import msgpack
import requests

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s %(levelname)s %(message)s",
    datefmt="%H:%M:%S",
)
log = logging.getLogger(__name__)

CHAIN_RPC = {
    "testnet": "https://rpc.hyperliquid-testnet.xyz/evm",
    "mainnet": "https://rpc.hyperliquid.xyz/evm",
}
BATCH_SIZE = 5  # blocks per RPC batch (x2 calls each = 10 RPC calls)
BATCH_DELAY = 5.0  # seconds between batches (~120 calls/min, conservative)
REQUEST_TIMEOUT = 30
RETRY_DELAY = 2
MAX_RETRIES = 5

# Highest precompile address transitions (from binary search on existing blocks)
PRECOMPILE_TRANSITIONS = [
    (44868476, bytes.fromhex("0000000000000000000000000000000000000813")),
    (42675776, bytes.fromhex("0000000000000000000000000000000000000812")),
    (41121887, bytes.fromhex("0000000000000000000000000000000000000811")),
    (0, bytes.fromhex("0000000000000000000000000000000000000810")),
]


def get_highest_precompile(block_num: int) -> bytes:
    for threshold, addr in PRECOMPILE_TRANSITIONS:
        if block_num >= threshold:
            return addr
    return PRECOMPILE_TRANSITIONS[-1][1]


def hex_to_bytes(h: str, length: int) -> bytes:
    val = int(h, 16) if h and h != "0x" else 0
    return val.to_bytes(length, "big")


def hex_to_raw(h: str) -> bytes:
    if not h or h == "0x":
        return b""
    h = h[2:] if h.startswith("0x") else h
    if len(h) % 2:
        h = "0" + h
    return bytes.fromhex(h)


def block_path(blocks_dir: str, block_num: int) -> str:
    m = (block_num - 1) // 1_000_000 * 1_000_000
    t = (block_num - 1) // 1_000 * 1_000
    return os.path.join(blocks_dir, str(m), str(t), f"{block_num}.rmp.lz4")


def rpc_to_nanoreth(block_json: dict, receipts_json: list) -> list:
    """Convert RPC JSON block + receipts to nanoreth Reth115 msgpack structure."""
    b = block_json
    bnum = int(b["number"], 16)

    header = {
        "parentHash": hex_to_raw(b["parentHash"]),
        "sha3Uncles": hex_to_raw(b["sha3Uncles"]),
        "miner": hex_to_raw(b["miner"]),
        "stateRoot": hex_to_raw(b["stateRoot"]),
        "transactionsRoot": hex_to_raw(b["transactionsRoot"]),
        "receiptsRoot": hex_to_raw(b["receiptsRoot"]),
        "logsBloom": hex_to_raw(b["logsBloom"]),
        "difficulty": hex_to_bytes(b.get("difficulty", "0x0"), 32),
        "number": hex_to_bytes(b["number"], 8),
        "gasLimit": hex_to_bytes(b["gasLimit"], 8),
        "gasUsed": hex_to_bytes(b["gasUsed"], 8),
        "timestamp": hex_to_bytes(b["timestamp"], 8),
        "extraData": hex_to_raw(b.get("extraData", "0x")),
        "mixHash": hex_to_raw(b.get("mixHash", "0x" + "00" * 32)),
        "nonce": hex_to_raw(b.get("nonce", "0x0000000000000000")),
        "baseFeePerGas": hex_to_bytes(b.get("baseFeePerGas", "0x0"), 8),
        "withdrawalsRoot": hex_to_raw(
            b.get("withdrawalsRoot", "0x" + "00" * 32)
        ),
        "blobGasUsed": hex_to_bytes(b.get("blobGasUsed", "0x0"), 8),
        "excessBlobGas": hex_to_bytes(b.get("excessBlobGas", "0x0"), 8),
        "parentBeaconBlockRoot": hex_to_raw(
            b.get("parentBeaconBlockRoot", "0x" + "00" * 32)
        ),
    }

    txs = []
    for tx in b.get("transactions", []):
        tx_type = tx.get("type", "0x0")
        sig = [
            hex_to_bytes(tx["r"], 32),
            hex_to_bytes(tx["s"], 32),
            hex_to_bytes(tx.get("yParity", tx.get("v", "0x0")), 8),
        ]

        if tx_type == "0x2":  # EIP-1559
            tx_inner = {
                "chainId": hex_to_bytes(tx["chainId"], 8),
                "nonce": hex_to_bytes(tx["nonce"], 8),
                "gas": hex_to_bytes(tx["gas"], 8),
                "maxFeePerGas": hex_to_bytes(tx["maxFeePerGas"], 16),
                "maxPriorityFeePerGas": hex_to_bytes(
                    tx["maxPriorityFeePerGas"], 16
                ),
                "to": hex_to_raw(tx["to"]) if tx.get("to") else b"",
                "value": hex_to_bytes(tx.get("value", "0x0"), 32),
                "accessList": [
                    {
                        "address": hex_to_raw(item["address"]),
                        "storageKeys": [
                            hex_to_raw(k)
                            for k in item.get("storageKeys", [])
                        ],
                    }
                    for item in tx.get("accessList", [])
                ],
                "input": hex_to_raw(tx.get("input", "0x")),
            }
            txs.append(
                {"signature": sig, "transaction": {"Eip1559": tx_inner}}
            )
        elif tx_type == "0x1":  # EIP-2930
            tx_inner = {
                "chainId": hex_to_bytes(tx["chainId"], 8),
                "nonce": hex_to_bytes(tx["nonce"], 8),
                "gasPrice": hex_to_bytes(tx["gasPrice"], 16),
                "gas": hex_to_bytes(tx["gas"], 8),
                "to": hex_to_raw(tx["to"]) if tx.get("to") else b"",
                "value": hex_to_bytes(tx.get("value", "0x0"), 32),
                "accessList": [
                    {
                        "address": hex_to_raw(item["address"]),
                        "storageKeys": [
                            hex_to_raw(k)
                            for k in item.get("storageKeys", [])
                        ],
                    }
                    for item in tx.get("accessList", [])
                ],
                "input": hex_to_raw(tx.get("input", "0x")),
            }
            txs.append(
                {"signature": sig, "transaction": {"Eip2930": tx_inner}}
            )
        else:  # Legacy (0x0)
            tx_inner = {
                "nonce": hex_to_bytes(tx["nonce"], 8),
                "gasPrice": hex_to_bytes(tx["gasPrice"], 16),
                "gas": hex_to_bytes(tx["gas"], 8),
                "to": hex_to_raw(tx["to"]) if tx.get("to") else b"",
                "value": hex_to_bytes(tx.get("value", "0x0"), 32),
                "input": hex_to_raw(tx.get("input", "0x")),
            }
            txs.append(
                {"signature": sig, "transaction": {"Legacy": tx_inner}}
            )

    rcpts = []
    for r in receipts_json or []:
        rtype = r.get("type", "0x0")
        type_name = {
            "0x0": "Legacy",
            "0x1": "Eip2930",
            "0x2": "Eip1559",
        }.get(rtype, "Legacy")
        logs = [
            {
                "address": hex_to_raw(l["address"]),
                "data": {
                    "topics": [hex_to_raw(t) for t in l.get("topics", [])],
                    "data": hex_to_raw(l.get("data", "0x")),
                },
            }
            for l in r.get("logs", [])
        ]
        rcpts.append(
            {
                "tx_type": type_name,
                "success": r.get("status") == "0x1",
                "cumulative_gas_used": int(
                    r.get("cumulativeGasUsed", "0x0"), 16
                ),
                "logs": logs,
            }
        )

    return [
        {
            "block": {
                "Reth115": {
                    "header": {
                        "hash": hex_to_raw(b["hash"]),
                        "header": header,
                    },
                    "body": {
                        "transactions": txs,
                        "ommers": [],
                        "withdrawals": [],
                    },
                },
            },
            "receipts": rcpts,
            "system_txs": [],
            "read_precompile_calls": [],
            "highest_precompile_address": get_highest_precompile(bnum),
        }
    ]


def rpc_call(session: requests.Session, rpc_url: str, batch: list) -> list:
    """Execute a batch JSON-RPC call with retries."""
    for attempt in range(MAX_RETRIES):
        try:
            resp = session.post(
                rpc_url, json=batch, timeout=REQUEST_TIMEOUT
            )
            resp.raise_for_status()
            results = resp.json()
            if isinstance(results, list):
                return results
            # Single dict = error (e.g. batch too large)
            if isinstance(results, dict) and "error" in results:
                raise requests.RequestException(
                    f"RPC error: {results['error']}"
                )
            return [results]
        except (requests.RequestException, json.JSONDecodeError) as e:
            if attempt < MAX_RETRIES - 1:
                delay = RETRY_DELAY * (2**attempt)
                log.warning(
                    "RPC error (attempt %d/%d): %s, retrying in %ds",
                    attempt + 1,
                    MAX_RETRIES,
                    e,
                    delay,
                )
                time.sleep(delay)
            else:
                raise


def find_missing_blocks(
    blocks_dir: str, start: int, end: int
) -> list[int]:
    """Find block numbers missing from the local cache."""
    missing = []
    for bnum in range(start, end + 1):
        path = block_path(blocks_dir, bnum)
        if not os.path.exists(path):
            missing.append(bnum)
    return missing


def fetch_and_write_batch(
    session: requests.Session,
    rpc_url: str,
    blocks_dir: str,
    block_nums: list[int],
) -> tuple[int, int]:
    """Fetch a batch of blocks from RPC and write to local cache.

    Returns (written, errors) count.
    """
    # Build batch request: getBlockByNumber + getBlockReceipts for each block
    batch = []
    for i, bnum in enumerate(block_nums):
        batch.append(
            {
                "jsonrpc": "2.0",
                "method": "eth_getBlockByNumber",
                "params": [hex(bnum), True],
                "id": i * 2,
            }
        )
        batch.append(
            {
                "jsonrpc": "2.0",
                "method": "eth_getBlockReceipts",
                "params": [hex(bnum)],
                "id": i * 2 + 1,
            }
        )

    results = rpc_call(session, rpc_url, batch)

    # Index results by id
    by_id = {r["id"]: r for r in results}

    written = 0
    errors = 0
    for i, bnum in enumerate(block_nums):
        block_resp = by_id.get(i * 2, {})
        receipt_resp = by_id.get(i * 2 + 1, {})

        block_data = block_resp.get("result")
        receipts_data = receipt_resp.get("result")

        if not block_data:
            log.warning("No block data for %d: %s", bnum, block_resp.get("error"))
            errors += 1
            continue

        if receipts_data is None:
            receipts_data = []

        try:
            converted = rpc_to_nanoreth(block_data, receipts_data)
            packed = msgpack.packb(converted, use_bin_type=True)
            compressed = lz4.frame.compress(packed)

            path = block_path(blocks_dir, bnum)
            os.makedirs(os.path.dirname(path), exist_ok=True)
            with open(path, "wb") as f:
                f.write(compressed)
            written += 1
        except Exception as e:
            log.error("Failed to convert block %d: %s", bnum, e)
            errors += 1

    return written, errors


def get_chain_tip(session: requests.Session, rpc_url: str) -> int:
    results = rpc_call(
        session,
        rpc_url,
        [{"jsonrpc": "2.0", "method": "eth_blockNumber", "params": [], "id": 0}],
    )
    return int(results[0]["result"], 16)


def find_cache_latest(blocks_dir: str) -> int:
    """Find the latest block number in the local cache."""
    millions = sorted(
        [d for d in os.listdir(blocks_dir) if d.isdigit()], key=int
    )
    if not millions:
        return 0
    latest_m = os.path.join(blocks_dir, millions[-1])
    thousands = sorted(
        [d for d in os.listdir(latest_m) if d.isdigit()], key=int
    )
    if not thousands:
        return 0
    latest_t = os.path.join(latest_m, thousands[-1])
    files = sorted(
        [f for f in os.listdir(latest_t) if f.endswith(".rmp.lz4")]
    )
    if not files:
        return 0
    return int(files[-1].replace(".rmp.lz4", ""))


def main():
    parser = argparse.ArgumentParser(
        description="Fetch missing blocks from RPC into local nanoreth cache"
    )
    parser.add_argument(
        "--rpc", default=None, help="RPC endpoint URL (default: per-chain public RPC)"
    )
    parser.add_argument(
        "--blocks-dir", required=True, help="Local blocks directory"
    )
    parser.add_argument(
        "--chain", choices=["testnet", "mainnet"], default="testnet",
        help="Chain to use (determines default RPC endpoint)"
    )
    parser.add_argument(
        "--start", type=int, default=None, help="Start block (default: cache latest + 1)"
    )
    parser.add_argument(
        "--end", type=int, default=None, help="End block (default: chain tip)"
    )
    parser.add_argument(
        "--batch-size", type=int, default=BATCH_SIZE, help="Blocks per RPC batch"
    )
    parser.add_argument(
        "--delay", type=float, default=BATCH_DELAY,
        help="Seconds between batches (rate limit protection)",
    )
    parser.add_argument(
        "--dry-run", action="store_true", help="Show what would be fetched"
    )
    parser.add_argument(
        "--scan-gaps", action="store_true",
        help="Scan for internal gaps in the range (slower)",
    )
    args = parser.parse_args()

    if args.rpc is None:
        args.rpc = CHAIN_RPC[args.chain]

    session = requests.Session()

    # Determine range
    if args.start is None:
        args.start = find_cache_latest(args.blocks_dir) + 1
        log.info("Cache latest: %d, starting from %d", args.start - 1, args.start)

    if args.end is None:
        args.end = get_chain_tip(session, args.rpc)
        log.info("Chain tip: %d", args.end)

    total_range = args.end - args.start + 1
    log.info("Range: %d -> %d (%d blocks)", args.start, args.end, total_range)

    # Find missing blocks
    if args.scan_gaps:
        log.info("Scanning for gaps in existing cache...")
        missing = find_missing_blocks(args.blocks_dir, args.start, args.end)
    else:
        # Assume everything from start to end is missing (faster for tip fill)
        missing = []
        for bnum in range(args.start, args.end + 1):
            path = block_path(args.blocks_dir, bnum)
            if not os.path.exists(path):
                missing.append(bnum)

    log.info("Missing blocks: %d / %d", len(missing), total_range)

    if args.dry_run:
        if missing:
            log.info(
                "Would fetch: %d -> %d (%d blocks)",
                missing[0],
                missing[-1],
                len(missing),
            )
        return

    if not missing:
        log.info("Nothing to fetch!")
        return

    # Fetch in batches
    total_written = 0
    total_errors = 0
    current_delay = args.delay
    consecutive_ok = 0
    t0 = time.time()

    for i in range(0, len(missing), args.batch_size):
        batch = missing[i : i + args.batch_size]
        try:
            written, errors = fetch_and_write_batch(
                session, args.rpc, args.blocks_dir, batch
            )
            total_written += written
            total_errors += errors
            consecutive_ok += 1
            # Gradually reduce delay back to baseline after success streak
            if consecutive_ok >= 5 and current_delay > args.delay:
                current_delay = max(args.delay, current_delay * 0.8)
                log.info("Reducing delay to %.1fs", current_delay)
        except requests.RequestException as e:
            log.warning("Batch failed: %s -- backing off to %.0fs", e, current_delay * 2)
            current_delay = min(current_delay * 2, 120)
            consecutive_ok = 0
            # Re-queue this batch by not advancing -- but simpler to just skip and re-run later
            total_errors += len(batch)

        elapsed = time.time() - t0
        progress = (i + len(batch)) / len(missing) * 100
        rate = total_written / elapsed if elapsed > 0 else 0
        remaining = (len(missing) - i - len(batch)) / rate if rate > 0 else 0

        if (i // args.batch_size) % 50 == 0 or i + len(batch) >= len(missing):
            log.info(
                "Progress: %d/%d (%.1f%%) | %d written, %d errors | "
                "%.1f blocks/s | ETA: %.0fs | delay: %.1fs",
                i + len(batch),
                len(missing),
                progress,
                total_written,
                total_errors,
                rate,
                remaining,
                current_delay,
            )

        # Rate limit protection
        if i + len(batch) < len(missing):
            time.sleep(current_delay)

    elapsed = time.time() - t0
    log.info(
        "Done: %d written, %d errors in %.1fs (%.1f blocks/s)",
        total_written,
        total_errors,
        elapsed,
        total_written / elapsed if elapsed > 0 else 0,
    )


if __name__ == "__main__":
    main()
