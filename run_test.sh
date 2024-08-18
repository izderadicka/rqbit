#!/bin/bash

TEST_NO=$1

for i in {1..100}; do
echo -n Running test $i
START=$SECONDS
RUST_LOG=debug RUST_BACKTRACE=1 cargo test -- test_e2e_download --nocapture > /tmp/test_log${TEST_NO}.txt 2>&1
FAIL_CODE=$?
if [ $FAIL_CODE -ne 0 ]; then
    echo " - Failed in $(( SECONDS - START )) secs with code $FAIL_CODE"
    exit $FAIL_CODE
    fi
echo " - Finished in $(( SECONDS - START )) secs"
done
