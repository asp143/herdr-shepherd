#!/bin/sh
# CliHerdr already passes the per-test socket path to every subprocess.
fixture_dir=${HERDR_SOCKET_PATH%/*}
if [ -f "$fixture_dir/argv" ]; then
  printf '%s\n' "$@" >> "$fixture_dir/argv"
fi
status=0
if [ -f "$fixture_dir/exit-code" ]; then
  IFS= read -r status < "$fixture_dir/exit-code"
fi
cat "$fixture_dir/response"
exit "$status"
