#!/bin/bash

set -e

export ETH_RPC_URL="${ETH_RPC_URL:-wss://hl-archive-node.xyz}"

success() {
    echo "Success: $1"
}

fail() {
    echo "Failed: $1"
    exit 1
}

ensure_cmd() {
    command -v "$1" > /dev/null 2>&1 || fail "$1 is required"
}

ensure_cmd jq
ensure_cmd cast
ensure_cmd wscat

if [[ ! "$ETH_RPC_URL" =~ ^wss?:// ]]; then
    fail "ETH_RPC_URL must be a websocket url"
fi

TITLE="Issue #78 - eth_getLogs should return system transactions"
cast logs \
    --rpc-url "$ETH_RPC_URL" \
    --from-block 15312567 \
    --to-block 15312570 \
    --address 0x9fdbda0a5e284c32744d2f17ee5c74b284993463 \
    0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef \
    | grep -q "0x00000000000000000000000020000000000000000000000000000000000000c5" \
    && success "$TITLE" || fail "$TITLE"

TITLE="Issue #78 - eth_getBlockByNumber should return the same logsBloom as official RPC"
OFFICIAL_RPC="https://rpc.hyperliquid.xyz/evm"
A=$(cast block 1394092 --rpc-url "$ETH_RPC_URL" -f logsBloom | md5sum)
B=$(cast block 1394092 --rpc-url "$OFFICIAL_RPC" -f logsBloom | md5sum)
echo node "$A"
echo rpc\  "$B"
[[ "$A" == "$B" ]] && success "$TITLE" || fail "$TITLE"

TITLE="eth_subscribe newHeads via wscat"
CMD='{"jsonrpc":"2.0","id":1,"method":"eth_subscribe","params":["newHeads"]}'
wscat -w 2 -c "$ETH_RPC_URL" -x "$CMD" | tail -1 | jq -r .params.result.nonce | grep 0x \
    && success "$TITLE" || fail "$TITLE"
