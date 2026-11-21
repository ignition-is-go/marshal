#!/usr/bin/env bash
set -eu

case "$1" in
  */tmux_adapter-*|tmux_adapter-*)
    if "$@"; then
      exit 0
    else
      original_status=$?
    fi
    (
      command -v strace >/dev/null || exit 0
      command -v timeout >/dev/null || exit 0
      traces="${RUNNER_TEMP:?}/native-tmux-processes"
      mkdir -p "$traces"
      cat /proc/self/limits > "$traces/limits-$$.txt"
      for resource in pids.max pids.current memory.max memory.current; do
        if [ -r "/sys/fs/cgroup/$resource" ]; then
          printf '%s=' "$resource"
          cat "/sys/fs/cgroup/$resource"
        fi
      done > "$traces/cgroup-$$.txt"
      timeout --kill-after=5s 60s strace -f -s 128 \
        -e trace=process,pipe,pipe2,dup,dup2,dup3,chdir,fchdir,prlimit64,setpgid \
        -e raw=execve,execveat,chdir \
        -o "$traces/trace-$$.log" "$@"
    ) || true
    exit "$original_status"
    ;;
  *) exec "$@" ;;
esac
