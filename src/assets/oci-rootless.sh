#!/bin/sh
# Rootless wrapper of an OCI runtime, written by kubernix. It patches the
# config.json of the bundle before running the runtime, see
# cri::write_rootless_wrappers for the details.
set -eu

bundle=""
prev=""
for arg in "$@"; do
  case "$prev" in --bundle | -b) bundle="$arg" ;; esac
  case "$arg" in --bundle=*) bundle="${arg#--bundle=}" ;; esac
  prev="$arg"
done

if [ -n "$bundle" ] && [ -f "$bundle/config.json" ]; then
  cfg="$bundle/config.json"
  tmp="$cfg.rootless"
  fail() {
    rm -f "$tmp"
    echo "oci-rootless: unable to patch $cfg" >&2
    exit 1
  }

  # The namespace types whose path points to a namespace of the runtime
  # itself, as JSON array entries.
  own=""
  paths=$(__JQ_PATH__ -r '.linux.namespaces[]? | select(.path) | "\(.type) \(.path)"' "$cfg") || fail
  while read -r type path; do
    case "$type" in
      network) ns=net ;;
      uts | ipc | pid) ns="$type" ;;
      *) continue ;;
    esac
    # shellcheck disable=SC3013 # supported by dash, bash and busybox
    if [ "$path" -ef "/proc/self/ns/$ns" ]; then
      own="$own\"$type\","
    fi
  done <<EOF
$paths
EOF

  # The OOM score cannot be lowered in a user namespace
  read -r oom </proc/self/oom_score_adj || fail

  # shellcheck disable=SC2016 # jq variables
  __JQ_PATH__ --argjson own "[${own%,}]" --argjson oom "$oom" '
    if (.process.oomScoreAdj? // $oom) < $oom
    then .process.oomScoreAdj = $oom
    else . end
    | if .linux.namespaces then
        .linux.namespaces |= map(select(.path == null or (.type | IN($own[]) | not)))
      else . end
    | if [.linux.namespaces[]? | select(.type == "uts")] | length == 0
      then .linux.namespaces = [{"type": "uts"}] + (.linux.namespaces // [])
      else . end
  ' "$cfg" >"$tmp" || fail
  mv "$tmp" "$cfg" || fail
fi

exec __RUNTIME_PATH__ "$@"
