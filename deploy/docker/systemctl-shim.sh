#!/bin/sh
# Stand-in for systemctl: nothing is ever "active" (stop the containers first).
case "$1" in
is-active) exit 3 ;;
*) echo "systemctl shim: unsupported: $*" >&2; exit 1 ;;
esac
