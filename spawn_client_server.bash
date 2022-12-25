#!/bin/bash
if [ "$#" -ne 1 ]; then
        echo "Usage: $0 <number_of_servers>"
        exit 1
fi

# Build client
cd client
RUSTFLAGS=-Awarnings cargo build 
cd ..

# Run server
./server.bash "$1" &

# Ensure server has time to setup
sleep 1 

# Run client
cd client
RUSTFLAGS=-Awarnings cargo run -- "$1"
