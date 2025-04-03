#!/bin/bash

set -e
set -o pipefail

gstorage_items=$(gcloud storage ls gs://mainnet-beta-ledger-us-ny5)
gstorage_snapshot_items=$(<<<"$gstorage_items" awk -F / '$(NF - 1) ~ /^[0-9]+$/')
gstorage_snapshot_latest_items=$(<<<"$gstorage_snapshot_items" sort -t / -k4 -n -r | head -3)

<<<"$gstorage_snapshot_latest_items" xargs -I@ gcloud storage cat --display-url @bounds.txt
