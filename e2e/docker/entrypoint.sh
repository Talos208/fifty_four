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

# IME に繋がらないときの手がかりを集める(失敗したときと、起動し直す前に呼ぶ)。
# 「XIM サーバは立っているか」「fcitx5 に入力コンテキストはあるか」「Zed の窓はフォーカスを持っているか」
ime_diag() {
    echo "---- ime diagnostics ($(date -u +%H:%M:%S))"
    if [ "$mode" = x11 ]; then
        echo "# XIM_SERVERS: $(timeout 5 xprop -root XIM_SERVERS 2>&1 | head -1)"
        echo "# focus window: $(timeout 5 xdotool getwindowfocus getwindowname 2>&1 | head -1)"
        echo "# zed windows: $(timeout 5 xdotool search --onlyvisible --class dev.zed.Zed 2>&1 | tr '\n' ' ')"
    else
        echo "# focused: $(timeout 5 swaymsg -t get_tree 2>&1 | grep -B3 -A3 '"focused": true' | grep -E 'app_id|name' | head -2 | tr -s ' \n' ' ')"
    fi
    echo "# fcitx5 DebugInfo:"
    timeout 5 dbus-send --session --print-reply --dest=org.fcitx.Fcitx5 /controller \
        org.fcitx.Fcitx.Controller1.DebugInfo 2>&1 | grep -E 'Group|IC \[' | head -10
    echo "# zed processes: $(pgrep -fc 'zed-editor' || true) (zed-editor)"
    # 画面も残す(Zed の窓が映っているか、ダイアログが出ていないか)
    timeout 10 e2e-shot "$out/failed-$1.png" 2>/dev/null && echo "# screenshot: /out/failed-$1.png"
}

on_exit() {
    local rc=$?
    [ "$rc" -eq 0 ] && return
    # 状態は 1 回だけ書く(2 回に分けると、ホスト側が最初の 1 回を読んで、理由を取り違える)。
    # Zed 本体が panic していれば、待ちのタイムアウトより根本の理由なので、それを書く
    if zed_panicked; then
        status "failed: Zed が panic した: $(grep -h -A1 panicked "$HOME/.local/share/zed/logs/Zed.log" | head -2 | tr '\n' ' ' | cut -c1-200)"
    else
        status "failed: $(tail -1 "$out/e2e.log")"
    fi
    log "failed (exit $rc). collecting logs"
    cp "$HOME/.local/share/zed/logs/Zed.log" "$out/Zed.log" 2>/dev/null || true
    {
        echo "---- processes"
        ps -eo pid,stat,etime,args | grep -vE ' (ps|grep) ' || true
        ime_diag final
        for f in xvfb.log sway.log fcitx5.log zed-cli.log Zed.log; do
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

zed_died=0
zed_panicked() { grep -q panicked "$HOME/.local/share/zed/logs/Zed.log" 2>/dev/null; }
wait_for() { # wait_for <秒> <説明> <コマンド...>
    local timeout=$1 what=$2; shift 2
    for _ in $(seq 1 $((timeout * 10))); do
        "$@" >/dev/null 2>&1 && return 0
        # Zed がもう起動しているなら、落ちていないかも見る(落ちていたら待つだけ無駄)。
        # CLI(zed --foreground)は、エディタ本体が panic したあとも残ることがあるので、pid だけでなく
        # Zed 本体のログの panic 行も見る
        if [ -n "${zed_pid:-}" ] && { ! kill -0 "$zed_pid" 2>/dev/null || zed_panicked; }; then
            zed_died=1
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
    # ソケットができただけでは出力(HEADLESS-1)の設定が終わっていない。Zed が先に繋ぐと落ちることがある
    wait_for 10 "sway output" sh -c 'swaymsg -t get_outputs | grep -q "\"name\": \"HEADLESS-1\""'
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
# fcitx5-remote が答えても、IME の frontend(XIM など)と mozc はまだ読み込み中のことがある。
# Zed が先に接続すると、XIM は一度失敗したまま IME に繋がらない(遅い環境で起きる)。最後に読む mozc まで待つ
wait_for "${E2E_FCITX5_TIMEOUT:-60}" "fcitx5 addons" grep -q 'Loaded addon mozc' "$out/fcitx5.log"
if [ "$mode" = x11 ]; then
    wait_for "${E2E_FCITX5_TIMEOUT:-60}" "XIM server" sh -c 'xprop -root XIM_SERVERS | grep -q fcitx'
fi

# Zed は端末から切り離さず起動し、ログを残す。
# 起動の途中で Zed が落ちる(Wayland で wayland/client.rs の unwrap が panic する。イメージをビルドした直後の
# 初回や、他のコンテナと同時に起動して負荷が高いときに出る。原因は未特定)ことがあるので、
# 落ちたら E2E_ZED_START_ATTEMPTS 回まで起動し直す。起動し直した事実は /out/zed-attempts に残す
# IME に繋がらないまま時間切れになったときも同じ。窓が出ないままの時間切れは起動し直さない(失敗にする)。
# (試験 A07 が警告する。黙って直して不安定さを隠さない)
bring_up_zed() {
    step "Zed (attempt $attempt)"
    zed --foreground "$HOME/workspace" "$HOME/workspace/test.txt" >"$out/zed-cli.log" 2>&1 &
    zed_pid=$!
    echo "$zed_pid" >"$out/zed.pid"
    wait_for "${E2E_ZED_WINDOW_TIMEOUT:-60}" "Zed window" e2e-window || return 1
    step "IME connection"
    # X11(XIM)では、Zed は最初のキー入力で初めて IME の入力コンテキストを作る。
    # それまでに打ったキーは IME を素通りするので、無害な Shift を押して作らせてから待つ
    local ime_ok=0
    if [ "$mode" = x11 ]; then
        wait_for "${E2E_IME_TIMEOUT:-20}" "IME connection" sh -c 'xdotool key shift && e2e-ime ready' && ime_ok=1
    else
        wait_for "${E2E_IME_TIMEOUT:-20}" "IME connection" e2e-ime ready && ime_ok=1
    fi
    [ "$ime_ok" = 1 ] && return 0
    # IME に繋がらないまま時間切れ。長く空けたあとの最初の起動などで、X11 でまれに起きる(原因は未特定。
    # 待ち続けても繋がらない)。Zed を起動し直すと繋がるかもしれないので、落ちたときと同じく起動し直す
    zed_died=1
    return 1
}

attempt=1
max_attempts=${E2E_ZED_START_ATTEMPTS:-3}
until zed_died=0; bring_up_zed; do
    if [ "$zed_died" != 1 ] || [ "$attempt" -ge "$max_attempts" ]; then
        exit 1 # タイムアウト、または起動し直しの上限
    fi
    log "Zed exited during startup (attempt $attempt/$max_attempts). restarting"
    ime_diag "attempt$attempt" | tee -a "$out/e2e.log" >&2
    # 残っているプロセス(CLI やエディタ本体)を止めてから、落ちた回のログを退避する
    pkill -f 'zed-editor|zed --foreground' 2>/dev/null || true
    sleep 1
    # 止まらなかったものが残ると、次の起動が残った本体に引き継がれてしまう(Zed は単一インスタンス)ので、強制終了する
    pkill -9 -f 'zed-editor|zed --foreground' 2>/dev/null || true
    # panic したのが「Zed が keymap を受け取る前に Modifiers が届いた」ことによるもの(下記)なら、
    # 起動し直しても同じ状態のまま落ち続けることがある。仮想キーボードを 1 回作って seat のキーボードを
    # 作り直させてから起動し直す(効果は統計的にしか確かめられていない)
    [ "$mode" = wayland ] && { e2e-key shift || true; sleep 1; }
    cp "$out/zed-cli.log" "$out/zed-cli.attempt$attempt.log" 2>/dev/null || true
    # 落ちた回の Zed.log は退避して、次の回のログに panic が混ざらないようにする(A04 が拾ってしまう)
    mv "$HOME/.local/share/zed/logs/Zed.log" "$out/Zed.attempt$attempt.log" 2>/dev/null || true
    attempt=$((attempt + 1))
done
echo "$attempt" >"$out/zed-attempts"

status ready
log "ready (E2E_DISPLAY=$mode, out=$out)"
trap - EXIT
exec "$@"
