#!/bin/bash

FAKEWORK=$1
MEAN=$2

ARGS="$FAKEWORK $MEAN calibrate"

sudo HOME=/home/users/fabricio LD_LIBRARY_PATH=$HOME/lib/x86_64-linux-gnu make test-system-rust TIMEOUT=120 LIBOS=catnip TEST=tcp-echo-multiflow ARGS="${ARGS}"
