#!/bin/bash

set -e

cluster="$1"
key="$2"

if [[ -z $cluster ]] || [[ -z $key ]]
then
    echo "Usage: $0 <cluster> <key>" >&2
    exit 1
fi

declare -A config=(
    [mainnet/GCLOUD_SNAPSHOTS]="gs://marinade-solana-snapshot-mainnet"
    [mainnet/LOCAL_SNAPSHOTS]="/mnt/snapshots-storage/snapshots"
    [mainnet/LOCAL_TEST_SNAPSHOTS]="/mnt/snapshots-storage/test-snapshots"
    [mainnet/WORKING_DIR]="/mnt/storage-1/snapshots"
    [mainnet/SLACK_FEED]="feed-snapshot"
    # Empty lets fetch-genesis.bash fall back to the agent's RPC_URL
    [mainnet/GENESIS_RPC_URL]=""
    # GENESIS_SHA256 is GENESIS_HASH in hex: the genesis hash is sha256(genesis.bin)
    [mainnet/GENESIS_HASH]="5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d"
    [mainnet/GENESIS_SHA256]="45296998a6f8e2a784db5d9f95e18fc23f70441a1039446801089879b08c7ef0"
    # Outside WORKING_DIR, which the scheduler prunes
    [mainnet/GENESIS_CACHE_DIR]="/mnt/storage-1/genesis-cache/mainnet"
    [mainnet/TIP_DISTRIBUTION_PROGRAM]="4R3gSG8BpU4t19KYj8CfnbtRpnT8gtk4dvTHxVRwc2r7"
    [mainnet/TIP_PAYMENT_PROGRAM]="T1pyyaTNZsKv2WcRAB8oVnk93mLJw2XzjtVYqCsaHqt"
    [mainnet/REQUIRE_PRIORITY_FEE_DATA]="true"
    [mainnet/ANNOTATION_STYLE]="error"

    [testnet/GCLOUD_SNAPSHOTS]="gs://marinade-solana-snapshot-testnet"
    [testnet/LOCAL_SNAPSHOTS]="/mnt/snapshots-storage/snapshots-testnet"
    [testnet/WORKING_DIR]="/mnt/storage-1/snapshots-testnet"
    [testnet/SLACK_FEED]="feed-snapshot-testnet"
    # Literal, not $RPC_URL: an agent environment hook owns that name and points it to mainnet
    [testnet/GENESIS_RPC_URL]="https://api.testnet.solana.com"
    # A testnet reset changes both hashes and fails the genesis fetch until they are updated here
    [testnet/GENESIS_HASH]="4uhcVJyU9pJkvQyS88uRDiswHXSCkY3zQawwpjk2NsNY"
    [testnet/GENESIS_SHA256]="3a132ece10305ec1830725502fa2b7e7eb8157e9123d4c1f654a71787161dc21"
    [testnet/GENESIS_CACHE_DIR]="/mnt/storage-1/genesis-cache/testnet"
    [testnet/TIP_DISTRIBUTION_PROGRAM]="DzvGET57TAgEDxvm3ERUM4GNcsAJdqjDLCne9sdfY4wf"
    [testnet/TIP_PAYMENT_PROGRAM]="GJHtFqM9agxPmkeKjHny6qiRKrXZALvvFGiKf11QE7hy"
    [testnet/REQUIRE_PRIORITY_FEE_DATA]="false"
    # Empty keeps the Buildkite default annotation style, which is grey
    [testnet/ANNOTATION_STYLE]=""
)

if [[ -z ${config["$cluster/$key"]+set} ]]
then
    echo "No '$key' configured for cluster '$cluster'" >&2
    exit 1
fi

echo "${config["$cluster/$key"]}"
