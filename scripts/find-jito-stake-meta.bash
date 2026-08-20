#!/bin/bash

set -e
set -o pipefail

search_dir="$1"

if [[ -z $search_dir ]]
then
    echo "Usage: $0 <search-dir>" >&2
    exit 1
fi

if ! [[ -d $search_dir ]]
then
    echo "Search directory ($search_dir) does not exist." >&2
    exit 1
fi

shopt -s nullglob
found=("$search_dir"/jito-stake-meta-*.json)
shopt -u nullglob

if [[ ${#found[@]} -eq 0 ]]
then
    echo "No Jito stake meta collection found in $search_dir!" >&2
    exit 1
fi

if [[ ${#found[@]} -gt 1 ]]
then
    echo "Several Jito stake meta collections found in $search_dir (${found[*]}), refusing to guess which one to use!" >&2
    exit 1
fi

name=$(basename "${found[0]}")
program_hash=${name#jito-stake-meta-}
program_hash=${program_hash%.json}

if [[ ! $program_hash =~ ^[0-9]{1,10}\.[0-9]{1,10}$ ]]
then
    echo "Jito stake meta collection '$name' is not named by a pair of dot-joined cksum CRC-32 decimals, which is the name the stakes ETL looks the object up by!" >&2
    exit 1
fi

echo "$name"
