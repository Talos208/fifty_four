#!/bin/bash
# 表示サーバ → D-Bus → IME → Zed の順に起動してから、引数のコマンドを実行する。
# 引数を省略すると(Dockerfile の CMD)待機し続けるので、docker exec で操作できる。
#
# E2E_DISPLAY で表示サーバを選ぶ:
#   x11     : Xvfb。IME は XIM 経由(変換中の文字は fcitx5 の小窓に出る)
#   wayland : sway(headless バックエンド)。IME は text-input-v3 ⇔ input-method-v2 経由
#             (変換中の文字は Zed のバッファ内にインライン表示される)
# 後から docker exec で入るプロセス向けに、接続先を /tmp/e2e-env に書き出す(e2e-* コマンドが読む)。
set -euo pipefail

out="${E2E_OUT:-/out}"
mkdir -p "$out" 2>/dev/null || out="$HOME/out"
mkdir -p "$out"
mode="${E2E_DISPLAY:-x11}"
size="${E2E_SCREEN%x*}" # 1600x1000x24 → 1600x1000

wait_for() { # wait_for <秒> <説明> <コマンド...>
    local timeout=$1 what=$2; shift 2
    for _ in $(seq 1 $((timeout * 10))); do
        "$@" >/dev/null 2>&1 && return 0
        sleep 0.1
    done
    echo "e2e: timeout waiting for $what" >&2
    return 1
}

case "$mode" in
x11)
    unset WAYLAND_DISPLAY
    Xvfb "$DISPLAY" -screen 0 "$E2E_SCREEN" -nolisten tcp >"$out/xvfb.log" 2>&1 &
    wait_for 10 Xvfb xdpyinfo
    printf 'export E2E_DISPLAY=x11 DISPLAY=%q\nunset WAYLAND_DISPLAY\n' "$DISPLAY" >/tmp/e2e-env
    ;;
wayland)
    # Zed が X11(XWayland)ではなく Wayland で起動するよう、DISPLAY は消しておく
    unset DISPLAY
    mkdir -p "$XDG_RUNTIME_DIR" && chmod 700 "$XDG_RUNTIME_DIR"
    sed "s/@SIZE@/$size/" "$HOME/.config/sway/config.in" >"$HOME/.config/sway/config"
    # headless: 実ディスプレイも入力デバイスもなしで動かす。pixman: GPU を使わず CPU で合成する
    WLR_BACKENDS=headless WLR_HEADLESS_OUTPUTS=1 WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1 \
        sway --config "$HOME/.config/sway/config" >"$out/sway.log" 2>&1 &
    wait_for 10 sway sh -c 'ls "$XDG_RUNTIME_DIR"/wayland-* "$XDG_RUNTIME_DIR"/sway-ipc.* >/dev/null 2>&1'
    WAYLAND_DISPLAY=$(basename "$(ls "$XDG_RUNTIME_DIR"/wayland-* | grep -v '\.lock$' | head -1)")
    SWAYSOCK=$(ls "$XDG_RUNTIME_DIR"/sway-ipc.* | head -1)
    export WAYLAND_DISPLAY SWAYSOCK
    printf 'export E2E_DISPLAY=wayland WAYLAND_DISPLAY=%q SWAYSOCK=%q\nunset DISPLAY\n' \
        "$WAYLAND_DISPLAY" "$SWAYSOCK" >/tmp/e2e-env
    ;;
*)
    echo "e2e: unknown E2E_DISPLAY=$mode (x11 | wayland)" >&2
    exit 2
    ;;
esac

# fcitx5 は D-Bus のセッションバスが要る。`docker exec` で後から入るプロセス(fcitx5-remote など)も
# 同じバスにつながるよう、アドレスは Dockerfile の ENV で固定しておき、そこに立てる
dbus-daemon --session --address="$DBUS_SESSION_BUS_ADDRESS" --fork --nopidfile

fcitx5 -d --replace >"$out/fcitx5.log" 2>&1
wait_for 10 fcitx5 fcitx5-remote

# Zed は端末から切り離さず起動し、ログを残す
zed --foreground "$HOME/workspace" "$HOME/workspace/test.txt" >"$out/zed.log" 2>&1 &
echo $! >"$out/zed.pid"
wait_for 60 "Zed window" e2e-window
# X11(XIM)では、Zed は最初のキー入力で初めて IME の入力コンテキストを作る。
# それまでに打ったキーは IME を素通りするので、無害な Shift を押して作らせてから待つ
if [ "$mode" = x11 ]; then
    wait_for 30 "IME connection" sh -c 'xdotool key shift && e2e-ime ready'
else
    wait_for 30 "IME connection" e2e-ime ready
fi

echo "e2e: ready (E2E_DISPLAY=$mode, out=$out)" >&2
exec "$@"
