#!/usr/bin/env bash
# @author Niko Wehr (wwwehr)
set -euo pipefail

# ---- config ----
BUCKET="hl-testnet-evm-blocks"
REGION="ap-northeast-1"
DEST="${HOME}/evm-blocks-testnet"
WORKERS=512
S3SYNC="${HOME}/.local/bin/s3sync"
START_AT=""   # default: run all
CHUNK_SIZE=1000000  # each prefix represents this many blocks
# ----------------

# parse args
while [[ $# -gt 0 ]]; do
  case "$1" in
    --start-at)
      START_AT="$2"
      shift 2
      ;;
    *)
      echo "Unknown arg: $1" >&2
      exit 1
      ;;
  esac
done

now(){ date +"%F %T"; }
log(){ printf '[%s] %s\n' "$(now)" "$*"; }
die(){ log "ERROR: $*"; exit 1; }
trap 'log "Signal received, exiting."; exit 2' INT TERM

need(){ command -v "$1" >/dev/null 2>&1 || die "missing dependency: $1"; }

install_s3sync_latest() {
  need curl
  GHAPI="https://api.github.com/repos/nidor1998/s3sync/releases/latest"

  os="$(uname | tr '[:upper:]' '[:lower:]')"
  arch_raw="$(uname -m)"
  case "$arch_raw" in
    x86_64|amd64) arch_tag="x86_64" ;;
    aarch64|arm64) arch_tag="aarch64" ;;
    *) die "unsupported arch: ${arch_raw}" ;;
  esac

  # Map OS → asset prefix
  case "$os" in
    linux)   prefix="s3sync-linux-glibc2.28-${arch_tag}" ;;
    darwin)  prefix="s3sync-macos-${arch_tag}" ;;
    msys*|mingw*|cygwin*|windows) prefix="s3sync-windows-${arch_tag}" ;;
    *) die "unsupported OS: ${os}" ;;
  esac

  # Fetch latest release JSON (unauthenticated)
  json="$(curl -fsSL "$GHAPI")" || die "failed to query GitHub API"

  # Pick URLs for tarball and checksum
  tar_url="$(printf '%s' "$json" | awk -F'"' '/browser_download_url/ {print $4}' | grep -F "${prefix}.tar.gz" | head -n1)"
  sum_url="$(printf '%s' "$json" | awk -F'"' '/browser_download_url/ {print $4}' | grep -F "${prefix}.sha256" | head -n1)"
  [[ -n "$tar_url" ]] || die "could not find asset for prefix: ${prefix}"
  [[ -n "$sum_url" ]] || die "could not find checksum for prefix: ${prefix}"

  mkdir -p "${HOME}/.local/bin"
  tmpdir="$(mktemp -d)"; trap 'rm -rf "$tmpdir"' EXIT
  tar_path="${tmpdir}/s3sync.tar.gz"
  sum_path="${tmpdir}/s3sync.sha256"

  log "Downloading: $tar_url"
  curl -fL --retry 5 --retry-delay 1 -o "$tar_path" "$tar_url"
  curl -fL --retry 5 --retry-delay 1 -o "$sum_path" "$sum_url"

  # Verify checksum
  want_sum="$(cut -d: -f2 <<<"$(sed -n 's/^sha256:\(.*\)$/\1/p' "$sum_path" | tr -d '[:space:]')" || true)"
  [[ -n "$want_sum" ]] || want_sum="$(awk '{print $1}' "$sum_path" || true)"
  [[ -n "$want_sum" ]] || die "could not parse checksum file"
  got_sum="$(sha256sum "$tar_path" | awk '{print $1}')"
  [[ "$want_sum" == "$got_sum" ]] || die "sha256 mismatch: want $want_sum got $got_sum"

  # Extract and install
  tar -xzf "$tar_path" -C "$tmpdir"
  binpath="$(find "$tmpdir" -maxdepth 2 -type f -name 's3sync' | head -n1)"
  [[ -x "$binpath" ]] || die "s3sync binary not found in archive"
  chmod +x "$binpath"
  mv -f "$binpath" "$S3SYNC"
  log "s3sync installed at $S3SYNC"
}

# --- deps & install/update ---
need aws
install_s3sync_latest
[[ ":$PATH:" == *":$HOME/.local/bin:"* ]] || export PATH="$HOME/.local/bin:$PATH"
mkdir -p "$DEST"

# list prefixes
log "Listing top-level prefixes in s3://${BUCKET}/"
mapfile -t PREFIXES < <(
  aws s3 ls "s3://${BUCKET}/" --region "$REGION" --request-payer requester \
  | awk '/^ *PRE /{print $2}' | sed 's:/$::' | grep -E '^[0-9]+$' || true
)
((${#PREFIXES[@]})) || die "No prefixes found."

# sort numerically to make order predictable
IFS=$'\n' read -r -d '' -a PREFIXES < <(printf '%s\n' "${PREFIXES[@]}" | sort -n && printf '\0')

# compute the effective start prefix:
# - if START_AT is set, floor it to the containing chunk boundary
effective_start=""
if [[ -n "$START_AT" ]]; then
  # numeric, base-10 safe
  start_num=$((10#$START_AT))
  chunk=$((10#$CHUNK_SIZE))
  effective_start=$(( (start_num / chunk) * chunk ))
fi

# mark initial status using numeric comparisons (no ordering assumptions)
declare -A RESULTS
for p in "${PREFIXES[@]}"; do
  if [[ -n "$effective_start" ]] && (( 10#$p < 10#$effective_start )); then
    RESULTS["$p"]="-- SKIPPED"
  else
    RESULTS["$p"]="-- TODO"
  fi
done

total_start=$(date +%s)

for p in "${PREFIXES[@]}"; do
  if [[ "${RESULTS[$p]}" == "-- SKIPPED" ]]; then
    continue
  fi
  src="s3://${BUCKET}/${p}/"
  dst="${DEST}/${p}/"
  mkdir -p "$dst"

  log "START  ${p}"
  start=$(date +%s)

  "$S3SYNC" \
    --source-request-payer \
    --source-region "$REGION" \
    --worker-size "$WORKERS" \
    --max-parallel-uploads "$WORKERS" \
    "$src" "$dst"

  end=$(date +%s)
  mins=$(( (end - start + 59) / 60 ))
  RESULTS["$p"]="$mins minutes"

  # Print status table so far
  echo "---- Status ----"
  for k in "${PREFIXES[@]}"; do
    echo "$k ${RESULTS[$k]}"
  done
  echo "----------------"
done

total_end=$(date +%s)
total_mins=$(( (total_end - total_start + 59) / 60 ))

echo "ALL DONE in $total_mins minutes."

