#!/bin/sh
case "$1 $2" in
  'pane run') exit 0 ;;
  'pane wait-output') printf '%s\n' 'matched readiness marker'; exit 0 ;;
  'agent start') exec "${0%/*}/herdr-output.sh" "$@" ;;
  *) exit 9 ;;
esac
