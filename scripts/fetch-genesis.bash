#!/bin/bash

set -e
set -o pipefail

target_dir="$1"
rpc_url_arg="$2"
cache_dir="$3"
genesis_hash="$4"
genesis_sha256="$5"

if [[ -z $target_dir ]]
then
    echo "Usage: $0 <target-dir> [rpc-url] [cache-dir genesis-hash genesis-sha256]" >&2
    exit 1
fi

if ! [[ -d $target_dir ]]
then
    echo "Target directory ($target_dir) does not exist." >&2
    exit 1
fi

rpc_url="${rpc_url_arg:-${RPC_URL:-https://api.mainnet-beta.solana.com}}"

download() {
    wget --retry-connrefused --waitretry=1 --tries=10 --timeout=30 -O "$1" "${rpc_url}/genesis.tar.bz2"
}

# The genesis hash is sha256(genesis.bin), so this proves the archive is the expected cluster's
is_expected_genesis() {
    local archive="$1"
    local scratch
    scratch=$(mktemp -d -p "$(dirname "$archive")")
    local ok=0
    tar -xjf "$archive" -C "$scratch" genesis.bin 2>/dev/null \
        && echo "$genesis_sha256  $scratch/genesis.bin" | sha256sum --check --status \
        || ok=1
    rm -r -- "$scratch"
    return $ok
}

cached="$cache_dir/$genesis_hash/genesis.tar.bz2"
if [[ -n $cache_dir && -n $genesis_hash && -n $genesis_sha256 ]] \
    && mkdir -p "$(dirname "$cached")" 2>/dev/null && [[ -w $(dirname "$cached") ]]
then
    if [[ -s $cached ]] && is_expected_genesis "$cached"
    then
        echo "Using cached genesis: $cached" >&2
    else
        echo "No valid cached genesis at $cached, downloading from $rpc_url" >&2
        partial=$(mktemp -p "$(dirname "$cached")" genesis.tar.bz2.XXXXXX)
        trap 'rm -f -- "$partial"' EXIT
        download "$partial"
        if ! is_expected_genesis "$partial"
        then
            echo "Genesis downloaded from $rpc_url is not the expected $genesis_hash, not caching it" >&2
            exit 1
        fi
        chmod 0644 -- "$partial"
        mv -- "$partial" "$cached"
        trap - EXIT
    fi
    cp -- "$cached" "$target_dir/genesis.tar.bz2"
else
    if [[ -n $cache_dir ]]
    then
        echo "WARNING: genesis cache $(dirname "$cached") is not writable, downloading without it" >&2
    fi
    download "$target_dir/genesis.tar.bz2"
fi

pv "$target_dir"/genesis.tar.bz2 | tar -xj -C "$target_dir"
