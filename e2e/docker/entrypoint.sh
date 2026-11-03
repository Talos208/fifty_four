#!/bin/bash
# 表示サーバ → D-Bus → IME → Zed の順に起動してから、引数のコマンドを実行する。
# 引数を省略すると(Dockerfile の CMD)待機し続けるので、docker exec で操作できる。
#
# E2E_DISPLAY で表示サーバを選ぶ:
#   x11     : Xvfb。IME は XIM 経由(変換中の文字は fcitx5 の小窓に出る)
#   wayland : sway(headless バックエンド)。IME は text-input-v3 ⇔ input-method-v2 経由
#             (変換中の文字は Zed のバッファ内にインライン表示される)
# 後から docker exec で入るプロセス向けに、接続先を /tmp/e2e-env に書き出す(e2e-* コマンドが読む)。
#
# 進み具合は stderr と $E2E_OUT/e2e.log の両方に出す(コンソールに stderr が出ない実行環境もあるため)。
# 状態は $E2E_OUT/e2e-status に 1 行で書く(starting: … / ready / failed: …)。
# 起動に失敗したら、各ログの末尾と Zed 本体のログを集めて出す。E2E_HOLD_ON_FAILURE=1(既定)なら
# コンテナを止めずに待機するので、exec で入って調べられる。CI では 0 にしてすぐ終了させる。
set -euo pipefail

out="${E2E_OUT:-/out}"
mkdir -p "$out" 2>/dev/null || out="$HOME/out"
mkdir -p "$out"
mode="${E2E_DISPLAY:-x11}"
size="${E2E_SCREEN%x*}" # 1600x1000x24 → 1600x1000
: >"$out/e2e.log"

log() { echo "e2e: $(date -u +%H:%M:%S) $*" | tee -a "$out/e2e.log" >&2; }
status() { echo "$*" >"$out/e2e-status"; }
step() { status "starting: $*"; log "starting: $*"; }

on_exit() {
    local rc=$?
    [ "$rc" -eq 0 ] && return
    status "failed: $(tail -1 "$out/e2e.log")"
    log "failed (exit $rc). collecting logs"
    cp "$HOME/.local/share/zed/logs/Zed.log" "$out/Zed.log" 2>/dev/null || true
    {
        echo "---- processes"
        ps -eo pid,stat,etime,args | grep -vE ' (ps|grep) ' || true
        for f in xvfb.log sway.log fcitx5.log zed.log Zed.log; do
            [ -s "$out/$f" ] || continue
            echo "---- $f (last 20 lines)"
            tail -20 "$out/$f"
        done
    } | tee -a "$out/e2e.log" >&2
    if [ "${E2E_HOLD_ON_FAILURE:-1}" = 1 ]; then
        log "holding the container for inspection (E2E_HOLD_ON_FAILURE=1)"
        exec sleep infinity
    fi
}
trap on_exit EXIT

wait_for() { # wait_for <秒> <説明> <コマンド...>
    local timeout=$1 what=$2; shift 2
    for _ in $(seq 1 $((timeout * 10))); do
        "$@" >/dev/null 2>&1 && return 0
        # Zed がもう起動しているなら、落ちていないかも見る(落ちていたら待つだけ無駄)
        if [ -n "${zed_pid:-}" ] && ! kill -0 "$zed_pid" 2>/dev/null; then
            log "Zed exited while waiting for $what"
            return 1
        fi
        sleep 0.1
    done
    log "timeout waiting for $what"
    return 1
}

step "display server ($mode)"
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
    log "unknown E2E_DISPLAY=$mode (x11 | wayland)"
    exit 2
    ;;
esac

# fcitx5 は D-Bus のセッションバスが要る。`docker exec` で後から入るプロセス(fcitx5-remote など)も
# 同じバスにつながるよう、アドレスは Dockerfile の ENV で固定しておき、そこに立てる
step "D-Bus and fcitx5"
dbus-daemon --session --address="$DBUS_SESSION_BUS_ADDRESS" --fork --nopidfile

fcitx5 -d --replace >"$out/fcitx5.log" 2>&1
wait_for 10 fcitx5 fcitx5-remote

# Zed は端末から切り離さず起動し、ログを残す
step "Zed"
zed --foreground "$HOME/workspace" "$HOME/workspace/test.txt" >"$out/zed.log" 2>&1 &
zed_pid=$!
echo "$zed_pid" >"$out/zed.pid"
wait_for 60 "Zed window" e2e-window
step "IME connection"
# X11(XIM)では、Zed は最初のキー入力で初めて IME の入力コンテキストを作る。
# それまでに打ったキーは IME を素通りするので、無害な Shift を押して作らせてから待つ
if [ "$mode" = x11 ]; then
    wait_for 30 "IME connection" sh -c 'xdotool key shift && e2e-ime ready'
else
    wait_for 30 "IME connection" e2e-ime ready
fi

status ready
log "ready (E2E_DISPLAY=$mode, out=$out)"
trap - EXIT
exec "$@"
