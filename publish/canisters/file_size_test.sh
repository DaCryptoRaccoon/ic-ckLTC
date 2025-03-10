#!/usr/bin/env bash

set -euo pipefail

file_size=$(wc -c <"$FILE")

if [ "$file_size" -ge "$MAX_SIZE" ]; then
    echo "'$FILE', '$file_size' bytes exceedes the allowed maximmum size '$MAX_SIZE'" >&2
    exit 1
else
    echo "'$FILE', '$file_size' bytes is below the allowed maximmum size '$MAX_SIZE'"
fi
