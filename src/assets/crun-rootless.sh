#!/bin/sh
command -v jq >/dev/null 2>&1 || { echo "crun-rootless: jq required but not found" >&2; exit 127; }
bundle=""
prev=""
for arg in "$@"; do
  if [ "$prev" = "--bundle" ]; then bundle="$arg"; fi
  case "$arg" in --bundle=*) bundle="${arg#--bundle=}" ;; esac
  prev="$arg"
done
if [ -n "$bundle" ] && [ -f "$bundle/config.json" ]; then
  cfg="$bundle/config.json"
  jq '
    del(.process.oomScoreAdj)
    | if [.linux.namespaces[]? | select(.type=="uts")] | length == 0
      then .linux.namespaces = [{"type":"uts"}] + .linux.namespaces
      else . end
  ' "$cfg" > "$cfg.tmp" && mv "$cfg.tmp" "$cfg"
  for m in network:net uts:uts ipc:ipc pid:pid; do
    oci="${m%%:*}"; proc="${m##*:}"
    self=$(stat -Lc %i "/proc/self/ns/$proc" 2>/dev/null) || continue
    path=$(jq -r ".linux.namespaces[]? | select(.type==\"$oci\" and .path) | .path" "$cfg" 2>/dev/null)
    [ -n "$path" ] || continue
    target=$(stat -Lc %i "$path" 2>/dev/null) || continue
    if [ "$self" = "$target" ]; then
      jq --arg t "$oci" --arg p "$path" \
        'del(.linux.namespaces[] | select(.type==$t and .path==$p))' \
        "$cfg" > "$cfg.tmp" && mv "$cfg.tmp" "$cfg"
    fi
  done
fi
exec __CRUN_PATH__ "$@"
