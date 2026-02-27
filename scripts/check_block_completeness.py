#!/usr/bin/env python3
"""Check local block cache for completeness against S3.

Scans each thousands-directory and reports missing blocks.
Uses the correct (height-1)-based bucketing to map block numbers to paths.

Usage:
    python scripts/check_block_completeness.py --blocks-dir /path/to/blocks
    python scripts/check_block_completeness.py --blocks-dir /path/to/blocks --start 40000000
    python scripts/check_block_completeness.py --blocks-dir /path/to/blocks --verbose
    python scripts/check_block_completeness.py --blocks-dir /path/to/blocks --fix
"""

import argparse
import os
import sys
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path

CHAIN_CONFIG = {
    "testnet": {"bucket": "hl-testnet-evm-blocks", "s3_start": 18_000_000},
    "mainnet": {"bucket": "hl-mainnet-evm-blocks", "s3_start": 1},
}


def expected_dir(block_number: int) -> tuple[int, int]:
    """Return (millions, thousands) directory for a block using (height-1) bucketing."""
    if block_number == 0:
        return (0, 0)
    millions = ((block_number - 1) // 1_000_000) * 1_000_000
    thousands = ((block_number - 1) // 1_000) * 1_000
    return (millions, thousands)


def cache_path(blocks_dir: Path, block_number: int) -> Path:
    m, t = expected_dir(block_number)
    return blocks_dir / str(m) / str(t) / f"{block_number}.rmp.lz4"


def find_latest_local_block(blocks_dir: Path) -> int:
    """Find the highest block number in the local cache."""
    best = 0
    for millions_dir in sorted(blocks_dir.iterdir()):
        if not millions_dir.is_dir() or not millions_dir.name.isdigit():
            continue
        for thousands_dir in sorted(millions_dir.iterdir()):
            if not thousands_dir.is_dir() or not thousands_dir.name.isdigit():
                continue
            for f in thousands_dir.iterdir():
                if f.suffix == ".lz4":
                    try:
                        bn = int(f.stem.split(".")[0])
                        if bn > best:
                            best = bn
                    except ValueError:
                        pass
    return best


def check_chunk(blocks_dir: Path, thousands_base: int, expected_blocks: list[int]) -> list[int]:
    """Check a single thousands-directory for missing blocks.

    Returns list of missing block numbers.
    """
    missing = []
    for bn in expected_blocks:
        if not cache_path(blocks_dir, bn).exists():
            missing.append(bn)
    return missing


def blocks_in_chunk(thousands_base: int, millions_base: int, latest: int) -> list[int]:
    """Return expected block numbers that map to this (millions, thousands) directory.

    A block B maps to dir (M, T) where:
        M = ((B-1) // 1_000_000) * 1_000_000
        T = ((B-1) // 1_000) * 1_000

    For a given T directory under M, blocks are T+1 through T+1000,
    filtered to those where the millions dir also matches.
    """
    blocks = []
    for bn in range(thousands_base + 1, thousands_base + 1001):
        if bn > latest:
            break
        m, t = expected_dir(bn)
        if m == millions_base and t == thousands_base:
            blocks.append(bn)
    return blocks


def download_blocks(blocks_dir: Path, missing: list[int], bucket: str, workers: int = 32) -> tuple[int, int]:
    """Download missing blocks from S3. Returns (downloaded, failed)."""
    import boto3
    from botocore.config import Config

    REGION = "ap-northeast-1"

    config = Config(region_name=REGION, max_pool_connections=workers + 5)
    s3 = boto3.client("s3", config=config)

    def download_one(bn: int) -> tuple[int, bool]:
        m, t = expected_dir(bn)
        key = f"{m}/{t}/{bn}.rmp.lz4"
        dest = cache_path(blocks_dir, bn)
        try:
            resp = s3.get_object(Bucket=bucket, Key=key, RequestPayer="requester")
            data = resp["Body"].read()
            dest.parent.mkdir(parents=True, exist_ok=True)
            dest.write_bytes(data)
            return (bn, True)
        except Exception:
            return (bn, False)

    downloaded = failed = 0
    with ThreadPoolExecutor(max_workers=workers) as pool:
        futures = {pool.submit(download_one, bn): bn for bn in missing}
        for fut in as_completed(futures):
            bn, ok = fut.result()
            if ok:
                downloaded += 1
            else:
                failed += 1
            done = downloaded + failed
            if done % 1000 == 0:
                print(f"  download progress: {done}/{len(missing)} "
                      f"({downloaded} ok, {failed} failed)")
    return downloaded, failed


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--blocks-dir", required=True,
                        help="Local blocks directory")
    parser.add_argument("--chain", choices=["testnet", "mainnet"], default="testnet",
                        help="Chain to use (determines S3 bucket and defaults)")
    parser.add_argument("--start", type=int, default=None,
                        help="First block to check (default: per-chain S3 start)")
    parser.add_argument("--end", type=int, default=0,
                        help="Last block to check (default: auto-detect latest)")
    parser.add_argument("--verbose", "-v", action="store_true",
                        help="Print every missing block number")
    parser.add_argument("--fix", action="store_true",
                        help="Download missing blocks from S3")
    parser.add_argument("--workers", type=int, default=32,
                        help="Parallel workers for --fix downloads")
    args = parser.parse_args()

    chain = CHAIN_CONFIG[args.chain]
    if args.start is None:
        args.start = chain["s3_start"]

    blocks_dir = Path(args.blocks_dir)

    print("Detecting latest local block...")
    latest = args.end if args.end > 0 else find_latest_local_block(blocks_dir)
    print(f"Checking blocks {args.start:,} to {latest:,}")

    total_expected = 0
    total_missing = 0
    all_missing: list[int] = []
    incomplete_chunks: list[tuple[int, int, int, int]] = []  # (millions, thousands, missing, expected)

    # Iterate over all (millions, thousands) combos in range
    millions_start = ((args.start - 1) // 1_000_000) * 1_000_000 if args.start > 0 else 0
    millions_end = ((latest - 1) // 1_000_000) * 1_000_000 if latest > 0 else 0

    for millions_base in range(millions_start, millions_end + 1, 1_000_000):
        # Determine thousands range within this millions dir
        t_start = max(((args.start - 1) // 1_000) * 1_000, millions_base) if args.start > 0 else millions_base
        t_end = min(millions_base + 999_000, ((latest - 1) // 1_000) * 1_000)

        for thousands_base in range(t_start, t_end + 1, 1_000):
            expected = blocks_in_chunk(thousands_base, millions_base, latest)
            if not expected:
                continue

            total_expected += len(expected)
            missing = check_chunk(blocks_dir, thousands_base, expected)

            if missing:
                total_missing += len(missing)
                all_missing.extend(missing)
                incomplete_chunks.append(
                    (millions_base, thousands_base, len(missing), len(expected))
                )

        # Progress per millions dir
        pct = (100 * (1 - total_missing / total_expected)) if total_expected else 100
        print(f"  {millions_base:>10,}/  checked -- "
              f"{total_expected:,} expected, {total_missing:,} missing "
              f"({pct:.2f}% complete)")

    print(f"\n{'='*60}")
    print(f"Total blocks expected: {total_expected:,}")
    print(f"Total blocks present:  {total_expected - total_missing:,}")
    print(f"Total blocks missing:  {total_missing:,}")
    pct = (100 * (1 - total_missing / total_expected)) if total_expected else 100
    print(f"Completeness:          {pct:.4f}%")

    if incomplete_chunks:
        print(f"\nIncomplete chunks: {len(incomplete_chunks)}")
        if args.verbose:
            print("\nMissing blocks:")
            for bn in sorted(all_missing)[:500]:
                print(f"  {bn}")
            if len(all_missing) > 500:
                print(f"  ... and {len(all_missing) - 500} more")
        else:
            # Show worst chunks
            worst = sorted(incomplete_chunks, key=lambda x: x[2], reverse=True)[:20]
            print("\nWorst chunks (most missing):")
            for m, t, miss, exp in worst:
                print(f"  {m}/{t}/  -- {miss}/{exp} missing")
            if len(incomplete_chunks) > 20:
                print(f"  ... and {len(incomplete_chunks) - 20} more chunks")

    if args.fix and all_missing:
        # Only fix blocks in S3 range
        fixable = [bn for bn in all_missing if bn >= chain["s3_start"]]
        unfixable = len(all_missing) - len(fixable)
        if unfixable:
            print(f"\n{unfixable} missing blocks below S3 range ({chain['s3_start']:,}) -- skipping")
        if fixable:
            print(f"\nDownloading {len(fixable):,} missing blocks from S3...")
            downloaded, failed = download_blocks(blocks_dir, fixable, chain["bucket"], args.workers)
            print(f"Done: {downloaded:,} downloaded, {failed:,} failed")

    return 1 if total_missing > 0 else 0


if __name__ == "__main__":
    sys.exit(main())
