#!/usr/bin/env bash
set -euo pipefail
script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
python3 -c 'import sys; sys.exit(0 if sys.version_info >= (3, 11) else "Python 3.11 or newer is required")'
case "$(uname -s)" in
  Linux|Darwin) ;;
  *) printf '%s\n' 'Agent integration currently supports Linux and macOS.' >&2; exit 1 ;;
esac
exec python3 -B "$script_dir/scripts/agent/install.py" "$@"
