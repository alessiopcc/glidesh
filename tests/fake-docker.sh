#!/bin/sh
# A minimal `docker` CLI stand-in for the container-module integration tests.
#
# Real Docker-in-Docker inside the test container is too heavy and too flaky to
# be worth it. This models just enough of the lifecycle — create, stop, start,
# unpause, remove, health status, logs, networks — to exercise the module's
# check/apply state machine, drift detection, and readiness gating over a real
# SSH session, and it records every invocation so tests can assert on the flags
# that actually reached the runtime.
#
# State lives under /var/lib/fakedocker:
#   c/<name>/status       one of running|exited|paused|dead
#   c/<name>/hash         the sh.glide.param-hash label
#   c/<name>/argv         the full `run` argument list
#   c/<name>/generation   bumped on every `run`, so tests can prove a container
#                         was reused rather than recreated
#   c/<name>/health       present only when the run declared a healthcheck
#   c/<name>/undeletable  when present, `rm` fails (models a stuck container)
set -u

# Overridable so the stub itself can be exercised outside the test container.
ROOT=${FAKE_DOCKER_ROOT:-/var/lib/fakedocker}
mkdir -p "$ROOT/c" "$ROOT/net"

cdir() { echo "$ROOT/c/$1"; }

cmd="${1:-}"
[ $# -gt 0 ] && shift

case "$cmd" in
container)
    [ "${1:-}" = inspect ] || exit 1
    shift
    fmt=""
    name=""
    while [ $# -gt 0 ]; do
        case "$1" in
        --format)
            fmt="$2"
            shift 2
            ;;
        *)
            name="$1"
            shift
            ;;
        esac
    done
    d=$(cdir "$name")
    [ -d "$d" ] || exit 1
    case "$fmt" in
    *State.Health.Status*)
        [ -f "$d/health" ] || exit 1
        cat "$d/health"
        ;;
    *State.Healthcheck.Status*)
        # Only the Docker-shaped field is modelled; the module must fall back
        # cleanly when this one is missing.
        exit 1
        ;;
    *param-hash*)
        cat "$d/hash" 2>/dev/null || true
        ;;
    *State.Status*)
        cat "$d/status"
        ;;
    *)
        exit 1
        ;;
    esac
    ;;

run)
    argv="$*"
    detach=no
    autorm=no
    name=""
    hash=""
    health=no
    exitcode=0
    while [ $# -gt 0 ]; do
        case "$1" in
        -d)
            detach=yes
            shift
            ;;
        --rm)
            autorm=yes
            shift
            ;;
        --name)
            name="$2"
            shift 2
            ;;
        --label)
            case "$2" in
            sh.glide.param-hash=*) hash="${2#sh.glide.param-hash=}" ;;
            esac
            shift 2
            ;;
        --health-cmd)
            health=yes
            shift 2
            ;;
        -e)
            # Tests drive a one-shot job's outcome through the environment.
            case "$2" in
            FAKE_EXIT=*) exitcode="${2#FAKE_EXIT=}" ;;
            FAKE_EXIT_ONCE=*)
                if [ -f "$ROOT/exit-once-used" ]; then
                    exitcode=0
                else
                    exitcode="${2#FAKE_EXIT_ONCE=}"
                    : >"$ROOT/exit-once-used"
                fi
                ;;
            esac
            shift 2
            ;;
        *)
            shift
            ;;
        esac
    done

    d=$(cdir "$name")
    if [ -d "$d" ]; then
        echo "Error response from daemon: Conflict. The container name \"/$name\" is already in use" >&2
        exit 125
    fi

    gen=$(cat "$ROOT/generation" 2>/dev/null || echo 0)
    gen=$((gen + 1))
    echo "$gen" >"$ROOT/generation"

    mkdir -p "$d"
    printf '%s' "$hash" >"$d/hash"
    printf '%s\n' "$argv" >"$d/argv"
    echo "$gen" >"$d/generation"
    echo "boot line one for $name" >"$d/logs"
    echo "boot line two for $name" >>"$d/logs"
    [ "$health" = yes ] && echo starting >"$d/health"

    if [ "$detach" = yes ]; then
        echo running >"$d/status"
        echo "fakeid-$name-$gen"
        exit 0
    fi

    echo exited >"$d/status"
    # `--rm` removes the container whatever the exit code, just like Docker.
    [ "$autorm" = yes ] && rm -rf "$d"
    echo "ran $name"
    exit "$exitcode"
    ;;

stop | start | unpause)
    name="$1"
    d=$(cdir "$name")
    [ -d "$d" ] || {
        echo "Error: No such container: $name" >&2
        exit 1
    }
    case "$cmd" in
    stop) echo exited >"$d/status" ;;
    *) echo running >"$d/status" ;;
    esac
    echo "$name"
    ;;

rm)
    while [ $# -gt 1 ]; do shift; done
    name="${1:-}"
    d=$(cdir "$name")
    [ -d "$d" ] || {
        echo "Error: No such container: $name" >&2
        exit 1
    }
    if [ -f "$d/undeletable" ]; then
        echo "Error response from daemon: container $name is in use" >&2
        exit 1
    fi
    rm -rf "$d"
    echo "$name"
    ;;

logs)
    while [ $# -gt 1 ]; do shift; done
    name="${1:-}"
    d=$(cdir "$name")
    [ -d "$d" ] || exit 1
    cat "$d/logs"
    ;;

network)
    sub="${1:-}"
    name="${2:-}"
    case "$sub" in
    inspect) [ -d "$ROOT/net/$name" ] || exit 1 ;;
    create) mkdir -p "$ROOT/net/$name" ;;
    *) exit 1 ;;
    esac
    ;;

*)
    exit 1
    ;;
esac
