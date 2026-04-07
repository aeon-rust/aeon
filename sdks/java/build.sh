#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

JAVA_HOME="${JAVA_HOME:-$(dirname "$(dirname "$(which java)")")}"
SRC=src/main/java
TEST=src/test/java
OUT=build/classes

echo "=== Aeon Java Processor SDK Build ==="
echo "Java: $(java -version 2>&1 | head -1)"

rm -rf "$OUT"
mkdir -p "$OUT"

echo "Compiling main sources..."
find "$SRC" -name "*.java" | xargs javac -d "$OUT"

echo "Compiling test sources..."
find "$TEST" -name "*.java" | xargs javac -cp "$OUT" -d "$OUT"

echo "Running tests..."
java -cp "$OUT" io.aeon.processor.AeonTest
