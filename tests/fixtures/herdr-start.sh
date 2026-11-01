#!/bin/sh
case "$1 $2" in
  'pane run') exit 0 ;;
  'pane wait-output') printf '%s\n' 'matched readiness marker'; exit 0 ;;
  'agent start') printf '%s\n' '{"result":{"agent":{"agent_status":"idle"}}}'; exit 0 ;;
  *) exit 9 ;;
esac
