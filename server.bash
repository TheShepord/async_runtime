#!/bin/bash
pids=()

if [ "$#" -ne 1 ]; then
	echo "Usage: $0 <number_of_servers>"
	exit 1
fi

# Spawn server listening on port 3000+i.
for ((i = 3000; i < 3000 + $1; i++))
do
	nc -l "${i}" <<< "hello from server" &
	pids+=("$!")
done
# server spawned by nc only sends its data once it closes.
# sleep a little bit to allow incoming messages to arrive
sleep 5
for pid in "${pids[@]}"
do
	kill "$pid"
done
