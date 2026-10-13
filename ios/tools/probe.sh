#!/bin/sh
# The local probe the debug app's automated flows dial (--uitest,
# --accepttest and the rest): an sshd on 127.0.0.1:2299 with throwaway
# keys in /tmp/ttp-ssh, and this checkout's debug server and CLI behind
# it, copied to /tmp/ttp-bin, with a HOME of their own in /tmp/ttp-srv.
#
#   ios/tools/probe.sh up         build the debug CLI and server, copy, start
#   ios/tools/probe.sh down       stop the sshd and the probe's server
#   ios/tools/probe.sh sshd-up    start only the sshd
#   ios/tools/probe.sh sshd-down  stop only the sshd (sessions stay)
#
# The binaries leave the checkout because a server that daemonises from
# under ~/Documents waits on macOS's prompt for that folder. Nothing here
# reaches the desktop's own server: the remote command clears the
# variables that name it and sets HOME, and processes are stopped by the
# pid files written here, never by name.
set -eu
cd "$(dirname "$0")/../.."
SSH=/tmp/ttp-ssh
SRV=/tmp/ttp-srv
BIN=/tmp/ttp-bin
PORT=2299

# A pid from a file, if that process is still the one the file is for.
owned() {
  [ -f "$1" ] || return 1
  pid=$(cat "$1")
  ps -ww -o command= -p "$pid" 2>/dev/null | grep -q "$2" || return 1
  echo "$pid"
}

sshd_down() {
  if pid=$(owned $SSH/sshd.pid "$SSH/sshd_config"); then
    kill "$pid"
  fi
}

server_down() {
  if pid=$(owned $SRV/.local/share/thinkterm/pid-debug "^$BIN/thinkterm-mux-server"); then
    kill "$pid"
  fi
}

sshd_up() {
  sshd_down
  user=$(id -un)
  env -i PATH=/usr/bin:/bin:/usr/sbin:/sbin HOME="$HOME" USER="$user" LOGNAME="$user" \
    /usr/sbin/sshd -f $SSH/sshd_config -E $SSH/sshd.log
}

up() {
  cargo build -p wezterm -p wezterm-mux-server
  mkdir -p $SSH $SRV $BIN
  chmod 700 $SSH $SRV $BIN
  server_down
  cp target/debug/thinkterm target/debug/thinkterm-mux-server $BIN/
  user=$(id -un)
  [ -f $SSH/hostkey ] || ssh-keygen -q -t ed25519 -N '' -C probe-host -f $SSH/hostkey
  # The app reads the user name from this key's "user@host" comment.
  [ -f $SSH/userkey ] || ssh-keygen -q -t ed25519 -N '' -C "$user@probe" -f $SSH/userkey
  cp $SSH/userkey.pub $SSH/authorized_keys
  cat > $SSH/sshd_config <<EOF
# Loopback-only sshd for the probe (throwaway keys).
Port $PORT
ListenAddress 127.0.0.1
HostKey $SSH/hostkey
PidFile $SSH/sshd.pid
AuthorizedKeysFile $SSH/authorized_keys
PubkeyAuthentication yes
PasswordAuthentication no
KbdInteractiveAuthentication no
UsePAM no
StrictModes no
AllowUsers $user
# The flows fail logins on purpose (unknown host keys); no lockout.
PerSourcePenalties no
# Sessions get the probe's home, not the desktop user's: what the app
# reads there (the desktop settings its tab icons come from) is the probe's.
SetEnv HOME=$SRV
EOF
  cat > $SSH/thinkterm-remote <<EOF
#!/bin/sh
# The probe's remote command: the copied debug CLI, against the probe's
# own server, never the desktop's.
unset WEZTERM_UNIX_SOCKET WEZTERM_PANE WEZTERM_CONFIG_FILE WEZTERM_CONFIG_DIR THINKTERM_CONFIG_FILE THINKTERM_CONFIG_DIR THINKTERM_MUX_SERVER_ID THINKTERM
export HOME=$SRV
cd $SRV || exit 1
exec $BIN/thinkterm "\$@"
EOF
  chmod 755 $SSH/thinkterm-remote
  sshd_up
  echo "probe up: sshd on 127.0.0.1:$PORT, server home $SRV"
}

case "${1:-up}" in
  up) up ;;
  down) sshd_down; server_down ;;
  sshd-up) sshd_up ;;
  sshd-down) sshd_down ;;
  *) echo "usage: $0 [up|down|sshd-up|sshd-down]" >&2; exit 2 ;;
esac
