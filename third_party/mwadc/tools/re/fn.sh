#!/bin/sh
# usage: fn.sh file.c name...
F=$1; shift
for n in "$@"; do awk -v n="$n" '/^\/\/ ==== /{p=($3==n)} p' "$F"; done
