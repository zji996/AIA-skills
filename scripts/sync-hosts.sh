#!/usr/bin/env bash
# Update AIA-skills on other machines over SSH: fast-forward their checkout and rerun install.sh,
# which also fetches the binaries matching the new bin.sha256. Hosts run in parallel; one line each.
#
#   sync-hosts.sh [--dir <remote path>] [<user@host>...] [-- <install.sh args>]
#
# Without hosts, reads ~/.config/aia-skills/hosts (one user@host per line, # comments allowed).
# The remote checkout must already exist (first time: run bootstrap.sh there). Exit 1 if any host failed.
set -euo pipefail

HOSTS_FILE="${AIA_SKILLS_HOSTS_FILE:-$HOME/.config/aia-skills/hosts}"
remote_dir='~/.local/share/aia-skills'
hosts=()
install_args=()

while (($#)); do
  case $1 in
    --dir) remote_dir=$2; shift 2 ;;
    -h|--help) sed -n '2,9p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    --) shift; install_args=("$@"); break ;;
    *) hosts+=("$1"); shift ;;
  esac
done
if ((${#hosts[@]} == 0)) && [[ -f $HOSTS_FILE ]]; then
  while IFS= read -r line; do
    line=${line%%#*}; line=${line//[[:space:]]/}
    [[ -n $line ]] && hosts+=("$line")
  done <"$HOSTS_FILE"
fi
((${#hosts[@]})) || { echo "sync-hosts: no hosts; pass user@host or list them in $HOSTS_FILE" >&2; exit 2; }

expected=$(git -C "$(dirname "$0")/.." rev-parse --short origin/main 2>/dev/null || true)
quoted_args=""
((${#install_args[@]})) && quoted_args=$(printf ' %q' "${install_args[@]}")
# The remote side prints its HEAD and the delegate version last, so the summary can show them.
remote="set -e; cd $remote_dir; git pull --ff-only -q; ./scripts/install.sh$quoted_args >/dev/null;
echo \"\$(git rev-parse --short HEAD) \$(skills/delegate/bin/delegate --version 2>/dev/null || echo 'delegate missing')\""

logs=$(mktemp -d)
trap 'rm -rf "$logs"' EXIT
for host in "${hosts[@]}"; do
  ssh -o BatchMode=yes -o ConnectTimeout=10 "$host" "$remote" >"$logs/$host.out" 2>&1 </dev/null &
  echo $! >"$logs/$host.pid"
done

failed=0
for host in "${hosts[@]}"; do
  if wait "$(cat "$logs/$host.pid")"; then
    summary=$(tail -n 1 "$logs/$host.out")
    note=""
    [[ -n $expected && $summary != "$expected "* ]] && note="  (origin/main here is $expected)"
    printf 'ok    %s  %s%s\n' "$host" "$summary" "$note"
  else
    failed=1
    printf 'FAIL  %s\n' "$host"
    sed 's/^/      /' "$logs/$host.out" | tail -n 8
  fi
done
exit $failed
