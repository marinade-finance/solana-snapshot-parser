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
    [mainnet/TIP_DISTRIBUTION_PROGRAM]="4R3gSG8BpU4t19KYj8CfnbtRpnT8gtk4dvTHxVRwc2r7"
    [mainnet/TIP_PAYMENT_PROGRAM]="T1pyyaTNZsKv2WcRAB8oVnk93mLJw2XzjtVYqCsaHqt"
    [mainnet/REQUIRE_PRIORITY_FEE_DATA]="true"
    [mainnet/LOCAL_SNAPSHOT_CP_ARGS]="--symbolic-link"

    [testnet/GCLOUD_SNAPSHOTS]="gs://marinade-solana-snapshot-testnet"
    [testnet/LOCAL_SNAPSHOTS]="/mnt/snapshots-storage/snapshots-testnet"
    [testnet/WORKING_DIR]="/mnt/storage-1/snapshots-testnet"
    [testnet/SLACK_FEED]="feed-snapshot-testnet"
    # Literal, not $RPC_URL: an agent environment hook owns that name and points it to mainnet
    [testnet/GENESIS_RPC_URL]="https://api.testnet.solana.com"
    [testnet/TIP_DISTRIBUTION_PROGRAM]="DzvGET57TAgEDxvm3ERUM4GNcsAJdqjDLCne9sdfY4wf"
    [testnet/TIP_PAYMENT_PROGRAM]="GJHtFqM9agxPmkeKjHny6qiRKrXZALvvFGiKf11QE7hy"
    [testnet/REQUIRE_PRIORITY_FEE_DATA]="false"
    # Copy, not symlink: agave opens the archive with O_NOATIME, which needs the agent to own it
    [testnet/LOCAL_SNAPSHOT_CP_ARGS]=""
)

if [[ -z ${config["$cluster/$key"]+set} ]]
then
    echo "No '$key' configured for cluster '$cluster'" >&2
    exit 1
fi

echo "${config["$cluster/$key"]}"
