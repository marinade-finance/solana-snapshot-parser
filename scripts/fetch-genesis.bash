#!/bin/bash

set -e
set -o pipefail

target_dir="$1"
rpc_url_arg="$2"
cache_dir="$3"
genesis_hash="$4"

if [[ -z $target_dir ]]
then
    echo "Usage: $0 <target-dir> [rpc-url] [cache-dir genesis-hash]" >&2
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

if [[ -n $cache_dir && -n $genesis_hash ]]
then
    # Keyed by the expected hash: a new hash (a reset cluster) starts an empty entry. The parser checks the
    # hash, so a wrong archive cached here fails the parse instead of being used; delete the entry then.
    cached="$cache_dir/$genesis_hash/genesis.tar.bz2"
    if [[ -s $cached ]]
    then
        echo "Using cached genesis: $cached" >&2
    else
        echo "No cached genesis at $cached, downloading from $rpc_url" >&2
        mkdir -p "$(dirname "$cached")"
        partial=$(mktemp -p "$(dirname "$cached")" genesis.tar.bz2.XXXXXX)
        trap 'rm -f -- "$partial"' EXIT
        download "$partial"
        # Renamed into place only once complete, so an interrupted download is never taken for a cached genesis
        chmod 0644 -- "$partial"
        mv -- "$partial" "$cached"
        trap - EXIT
    fi
    cp -- "$cached" "$target_dir/genesis.tar.bz2"
else
    download "$target_dir/genesis.tar.bz2"
fi

pv "$target_dir"/genesis.tar.bz2 | tar -xj -C "$target_dir"
