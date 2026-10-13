#!/usr/bin/env bash
source ./helper.sh

LIMIT=10

# The label is fixed.
label() {
  echo "circle"
}

describe() {
  label
  printf '%s\n' "$LIMIT"
}

describe
