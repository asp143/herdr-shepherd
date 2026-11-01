#!/bin/sh
# CliHerdr already passes the per-test socket path to every subprocess.
exec cat "${HERDR_SOCKET_PATH%/*}/response"
