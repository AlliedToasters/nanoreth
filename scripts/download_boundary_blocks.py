#!/usr/bin/env python3
"""Download missing boundary blocks (multiples of 1000) from S3.

Our BlockCache previously used an incorrect path formula for these blocks,
so they were never downloaded. This script fetches them using the correct
S3 key: (height-1)/1000000 and (height-1)/1000 for directory bucketing.

Usage:
    python scripts/download_boundary_blocks.py --blocks-dir /path/to/blocks
    python scripts/download_boundary_blocks.py --blocks-dir /path/to/blocks --workers 32 --dry-run
"""

import argparse
import os
import sys
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path

import boto3
from botocore.config import Config

CHAIN_CONFIG = {
    "testnet": {"bucket": "hl-testnet-evm-blocks", "default_start": 18_000_000, "default_end": 46_000_000},
    "mainnet": {"bucket": "hl-mainnet-evm-blocks", "default_start": 1, "default_end": 28_000_000},
}
REGION = "ap-northeast-1"


def s3_key(block_number: int) -> str:
    """Correct S3 key using (height-1) bucketing."""
    millions = ((block_number - 1) // 1_000_000) * 1_000_000
    thousands = ((block_number - 1) // 1_000) * 1_000
    return f"{millions}/{thousands}/{block_number}.rmp.lz4"


def cache_path(blocks_dir: Path, block_number: int) -> Path:
    """Local cache path matching S3 key layout."""
    millions = ((block_number - 1) // 1_000_000) * 1_000_000
    thousands = ((block_number - 1) // 1_000) * 1_000
    return blocks_dir / str(millions) / str(thousands) / f"{block_number}.rmp.lz4"


def find_missing_blocks(blocks_dir: Path, start: int, end: int) -> list[int]:
    """Find boundary blocks that are missing from local cache."""
    missing = []
    for h in range(start, end + 1, 1000):
        if not cache_path(blocks_dir, h).exists():
            missing.append(h)
    return missing


def download_block(s3_client, blocks_dir: Path, block_number: int, bucket: str) -> tuple[int, bool, str]:
    """Download a single block from S3. Returns (block, success, message)."""
    key = s3_key(block_number)
    dest = cache_path(blocks_dir, block_number)
    try:
        resp = s3_client.get_object(
            Bucket=bucket, Key=key, RequestPayer="requester"
        )
        data = resp["Body"].read()
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_bytes(data)
        return (block_number, True, f"{len(data)} bytes")
    except Exception as e:
        return (block_number, False, str(e))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--blocks-dir", required=True,
                        help="Local blocks directory")
    parser.add_argument("--chain", choices=["testnet", "mainnet"], default="testnet",
                        help="Chain to use (determines S3 bucket and defaults)")
    parser.add_argument("--start", type=int, default=None,
                        help="First boundary block (default: per-chain S3 start)")
    parser.add_argument("--end", type=int, default=None,
                        help="Last boundary block to check (default: per-chain)")
    parser.add_argument("--workers", type=int, default=32,
                        help="Parallel download threads")
    parser.add_argument("--dry-run", action="store_true",
                        help="Only count missing blocks, don't download")
    args = parser.parse_args()

    chain = CHAIN_CONFIG[args.chain]
    if args.start is None:
        args.start = chain["default_start"]
    if args.end is None:
        args.end = chain["default_end"]

    blocks_dir = Path(args.blocks_dir)

    print(f"Scanning for missing boundary blocks [{args.start:,} - {args.end:,}] ({args.chain})...")
    missing = find_missing_blocks(blocks_dir, args.start, args.end)
    print(f"Found {len(missing):,} missing boundary blocks")

    if args.dry_run or not missing:
        return

    config = Config(region_name=REGION, max_pool_connections=args.workers + 5)
    s3 = boto3.client("s3", config=config)

    downloaded = 0
    failed = 0
    with ThreadPoolExecutor(max_workers=args.workers) as pool:
        futures = {pool.submit(download_block, s3, blocks_dir, h, chain["bucket"]): h for h in missing}
        for future in as_completed(futures):
            block_number, success, msg = future.result()
            if success:
                downloaded += 1
            else:
                failed += 1
                print(f"  FAIL block {block_number}: {msg}", file=sys.stderr)
            if (downloaded + failed) % 500 == 0:
                total = downloaded + failed
                print(f"  Progress: {total:,}/{len(missing):,} "
                      f"({downloaded:,} ok, {failed:,} failed)")

    print(f"\nDone: {downloaded:,} downloaded, {failed:,} failed "
          f"out of {len(missing):,} missing")


if __name__ == "__main__":
    main()
