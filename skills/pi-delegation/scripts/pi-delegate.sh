#!/bin/sh
# Compatibility entry point; the implementation is pi_delegate.py (Python 3.9+ standard library).
exec python3 "$(dirname "$0")/pi_delegate.py" "$@"
