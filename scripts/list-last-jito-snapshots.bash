#!/bin/bash#!/bin/bash

set -e
set -o pipefail

gstorage_items=$(gcloud storage ls --recursive gs://jito-mainnet)
