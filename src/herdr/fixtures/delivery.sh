#!/bin/sh
# Isolated CLI transport fixture; never invokes Herdr.
fixture_dir=${HERDR_SOCKET_PATH%/*}
printf '%s\n' "$@" >> "$fixture_dir/requests"
case "$1 $2" in
  'agent get') cat "$fixture_dir/recipient" ;;
  'pane get') cat "$fixture_dir/session" ;;
  'agent prompt')
    status=0
    if [ -f "$fixture_dir/exit-code" ]; then
      IFS= read -r status < "$fixture_dir/exit-code"
    fi
    cat "$fixture_dir/response"
    exit "$status" ;;
  *) exit 9 ;;
esac
