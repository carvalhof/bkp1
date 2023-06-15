#!/bin/bash

LOCAL='192.168.100.2:12345'
SERVER_CPUS=$1
SERVER_ARGS=$2

SERVER_LCORES=`echo ${SERVER_CPUS} | sed 's/,/:/g'`

ARGS="--server ${LOCAL} ${SERVER_LCORES} ${SERVER_ARGS}"

#taskset -c 0 sudo perf stat -e cache-references:u,cache-misses:u,cycles:u,instructions:u -A -d -d -d -C ${SERVER_CPUS} -I 1000 -o output.perf &
taskset -c 0 sudo perf stat -e cycles:u,instructions:u,cache-references:u,cache-misses:u,bus-cycles:u,L1-dcache-loads:u,L1-dcache-load-misses:u,L1-dcache-stores:u,dTLB-loads:u,dTLB-load-misses:u,iTLB-loads:u,iTLB-load-misses:u,LLC-loads:u,LLC-load-misses:u,LLC-stores:u -A -C ${SERVER_CPUS} -I 1000 -o output.perf &

#taskset -c ${SERVER_CPUS} sudo HOME=/home/users/fabricio LD_LIBRARY_PATH=$HOME/lib/x86_64-linux-gnu make test-system-rust LIBOS=catnip TEST=tcp-echo-multiflow ARGS="${ARGS}" RUST_LOG=trace
#taskset -c ${SERVER_CPUS} sudo HOME=/home/users/fabricio LD_LIBRARY_PATH=$HOME/lib/x86_64-linux-gnu make test-system-rust LIBOS=catnip TEST=tcp-echo-multiflow ARGS="${ARGS}" RUST_BACKTRACE=1
taskset -c ${SERVER_CPUS} sudo HOME=/home/users/fabricio LD_LIBRARY_PATH=$HOME/lib/x86_64-linux-gnu make test-system-rust LIBOS=catnip TEST=tcp-echo-multiflow ARGS="${ARGS}"
