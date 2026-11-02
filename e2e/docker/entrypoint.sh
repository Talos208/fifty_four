#!/bin/bash
# 仮想ディスプレイ → D-Bus → IME → Zed の順に起動してから、引数のコマンドを実行する。
# 引数を省略すると(Dockerfile の CMD)待機し続けるので、docker exec で操作できる。
set -euo pipefail

out="${E2E_OUT:-/out}"
mkdir -p "$out" 2>/dev/null || out="$HOME/out"
mkdir -p "$out"

wait_for() { # wait_for <秒> <説明> <コマンド...>
    local timeout=$1 what=$2; shift 2
    for _ in $(seq 1 $((timeout * 10))); do
        "$@" >/dev/null 2>&1 && return 0
        sleep 0.1
    done
    echo "e2e: timeout waiting for $what" >&2
    return 1
}

Xvfb "$DISPLAY" -screen 0 "$E2E_SCREEN" -nolisten tcp >"$out/xvfb.log" 2>&1 &
wait_for 10 Xvfb xdpyinfo

# fcitx5 は D-Bus のセッションバスが要る。以降の子プロセスもこのバスを共有する
eval "$(dbus-launch --sh-syntax)"
export DBUS_SESSION_BUS_ADDRESS

fcitx5 -d --replace >"$out/fcitx5.log" 2>&1
wait_for 10 fcitx5 fcitx5-remote

# Zed は端末から切り離さず起動し、ログを残す
zed --foreground "$HOME/workspace" "$HOME/workspace/test.txt" >"$out/zed.log" 2>&1 &
echo $! >"$out/zed.pid"
wait_for 60 "Zed window" xdotool search --onlyvisible --class dev.zed.Zed

echo "e2e: ready (DISPLAY=$DISPLAY, out=$out)" >&2
exec "$@"
