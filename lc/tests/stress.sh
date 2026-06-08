#!/usr/bin/env bash
# Stress / availability test. Requires `siege` (brew install siege).
# Usage: ./tests/stress.sh [url] [concurrency] [duration]
set -u
URL="${1:-http://127.0.0.1:8080/}"
CONC="${2:-50}"
DUR="${3:-30S}"

echo "siege -b -c $CONC -t $DUR $URL"
echo "(availability must be >= 99.5%)"
siege -b -c "$CONC" -t "$DUR" "$URL"

echo
echo "Watch for fd leaks while this runs, e.g.:"
echo "  watch -n1 'lsof -p \$(pgrep -f target/debug/localhost) | wc -l'"
echo "Watch memory:"
echo "  watch -n1 'ps -o rss= -p \$(pgrep -f target/debug/localhost)'"
