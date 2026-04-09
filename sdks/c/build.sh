#!/usr/bin/env bash
set -e
cd "$(dirname "$0")"
mkdir -p build
cc -Wall -Wextra -std=c11 -I include -c src/aeon_processor.c -o build/aeon_processor.o
cc -Wall -Wextra -std=c11 -I include test/test_aeon.c build/aeon_processor.o -o build/test_aeon
echo "Build complete. Running tests..."
./build/test_aeon
