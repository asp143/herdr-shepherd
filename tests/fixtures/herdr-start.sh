#!/bin/sh
case "$1 $2" in
  'pane run') exit 0 ;;
  'pane wait-output') printf '%s\n' 'matched readiness marker'; exit 0 ;;
  'agent prompt')
    printf '%s\n' "$@" > "${HERDR_SOCKET_PATH%/*}/prompt-args"
    printf '%s\n' '{"result":{"agent":{"agent_status":"working"}}}'; exit 0 ;;
  'agent start')
    printf '%s\n' "$@" > "${HERDR_SOCKET_PATH%/*}/start-args"
    exec "${0%/*}/herdr-output.sh" "$@" ;;
  *) exit 9 ;;
esac
