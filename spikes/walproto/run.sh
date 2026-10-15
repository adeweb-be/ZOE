#!/usr/bin/env bash
# The commit matrix of ZOE-184, second run: the fair leader election, the
# fold numbers fixed, and the variant that compresses on the member.
set -euo pipefail
W=/tmp/claude-0/-home-user-ZOE/b000b127-b63d-59d4-85d5-796e54212bc0/scratchpad/walproto/target/release/walproto
OUT="$1"
dir=/home/user/ZOE/target/walproto-run
cell() { # scenario size c txns
  $W commit layout=main scenario=$1 size=$2 c=$3 txns=$4 dir=$dir >> "$OUT"
  $W commit layout=log scenario=$1 size=$2 c=$3 txns=$4 dir=$dir threshold=64 >> "$OUT"
  $W commit layout=log scenario=$1 size=$2 c=$3 txns=$4 dir=$dir threshold=100000 | sed 's/"layout":"log"/"layout":"log-nofold"/' >> "$OUT"
  $W commit layout=log scenario=$1 size=$2 c=$3 txns=$4 dir=$dir threshold=64 precompress=1 | sed 's/"layout":"log"/"layout":"log-pre"/' >> "$OUT"
}
for round in 1 2; do
  for scenario in add update; do
    cell $scenario 1024 1 200
    cell $scenario 1024 16 20
    cell $scenario 1024 64 6
  done
done
for scenario in add update; do
  cell $scenario 102400 1 30
  cell $scenario 102400 16 4
  cell $scenario 102400 64 2
done
echo "matrix done" >> "$OUT"
for table in 0 10000; do
  $W reads table=$table size=1024 dir=$dir >> "$OUT.reads"
done
$W reads table=1000000 size=100 dir=$dir >> "$OUT.reads"
$W reads table=0 size=1024 under_fold=1 dir=$dir >> "$OUT.reads"
for mb in 64 256; do
  $W recovery mb=$mb size=1024 dir=$dir >> "$OUT.recovery"
done
echo "all done" >> "$OUT"
