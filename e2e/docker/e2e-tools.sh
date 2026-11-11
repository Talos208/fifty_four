#!/bin/bash
# 表示サーバ(X11 / Wayland)の違いを吸収する操作コマンド。呼び出し名で動作が変わる:
#   e2e-key <キー>...      例: e2e-key ctrl+End Return space  (キー名は X の keysym 名)
#   e2e-type <文字列>       ASCII を1文字ずつ打つ(IME が ON ならローマ字入力になる)
#   e2e-shot <出力.png>     画面全体のスクリーンショット
#   e2e-claude-models       コンテナの claude が「選べる」と答えるモデルの表示名を、1 行に 1 つ出す
#                           (stream-json の initialize 応答の models。ACP のセレクタと突き合わせる正解)
#   e2e-ime on|off|state    IME の ON/OFF(fcitx5 は D-Bus 経由なので表示サーバに依らない)
#   e2e-ime ready           フォーカスを持つ入力コンテキストが fcitx5 にあれば 0 で終わる
#                           (Zed は窓を出した少し後に IME とつながる。その前に打つと IME を素通りする)
#   e2e-window              Zed のウィンドウが表示されていれば 0 で終わる
set -euo pipefail
# shellcheck disable=SC1091
[ -f /tmp/e2e-env ] && . /tmp/e2e-env
mode="${E2E_DISPLAY:-x11}"
delay="${E2E_KEY_DELAY_MS:-50}"

# "ctrl+shift+End" → wtype の引数(-M ctrl -M shift -k End -m shift -m ctrl)
wtype_args() {
    local IFS=+ parts=() mods=() key i
    read -ra parts <<<"$1"
    key="${parts[-1]}"
    for ((i = 0; i < ${#parts[@]} - 1; i++)); do mods+=("${parts[i]}"); done
    for m in "${mods[@]}"; do printf '%s\n' -M "$m"; done
    printf '%s\n' -k "$key"
    for ((i = ${#mods[@]} - 1; i >= 0; i--)); do printf '%s\n' -m "${mods[i]}"; done
}

case "$(basename "$0")" in
e2e-key)
    for k in "$@"; do
        if [ "$mode" = wayland ]; then
            mapfile -t args < <(wtype_args "$k")
            wtype "${args[@]}"
        else
            xdotool key "$k"
        fi
        sleep "$(awk "BEGIN{print $delay/1000}")"
    done
    ;;
e2e-type)
    if [ "$mode" = wayland ]; then
        wtype -d "$delay" "$1"
    else
        xdotool type --delay "$delay" "$1"
    fi
    ;;
e2e-shot)
    if [ "$mode" = wayland ]; then grim "$1"; else import -window root "$1"; fi
    ;;
e2e-claude-models)
    # 本体(`fifty_four_lsp --acp`)と同じ引数で、プロンプトは送らず initialize だけ送る。
    # 応答が出たら待たずに止める(最大 E2E_CLAUDE_TIMEOUT 秒、既定 30)
    cd "$HOME/workspace"
    req='{"type":"control_request","request_id":"e2e","request":{"subtype":"initialize"}}'
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    mkfifo "$tmp/in"
    # stdin を閉じると応答の前に claude が終わることがあるので、応答が出るまで開けておく
    claude -p --input-format stream-json --output-format stream-json --verbose \
        --no-session-persistence --setting-sources project <"$tmp/in" >"$tmp/out" 2>/dev/null &
    cpid=$!
    exec 3>"$tmp/in"
    echo "$req" >&3
    for _ in $(seq 1 $((${E2E_CLAUDE_TIMEOUT:-30} * 10))); do
        grep -q '"control_response"' "$tmp/out" && break
        sleep 0.1
    done
    exec 3>&-
    kill "$cpid" 2>/dev/null || true
    wait "$cpid" 2>/dev/null || true
    resp=$(grep -m1 '"control_response"' "$tmp/out" || true)
    [ -n "$resp" ] || { echo "claude が initialize に答えない" >&2; exit 1; }
    # `models` の中の displayName だけを取る(commands など他の場所の displayName と混ざらないように)
    printf '%s' "$resp" | sed 's/.*"models":\[//' | grep -o '"displayName":"[^"]*"' | sed 's/^"displayName":"//; s/"$//'
    ;;
e2e-ime)
    case "${1:-state}" in
    on) fcitx5-remote -o ;;
    off) fcitx5-remote -c ;;
    state) fcitx5-remote ;; # 1: OFF, 2: ON
    ready)
        dbus-send --session --print-reply --dest=org.fcitx.Fcitx5 /controller \
            org.fcitx.Fcitx.Controller1.DebugInfo | grep -q 'focus:1'
        ;;
    esac
    ;;
e2e-window)
    if [ "$mode" = wayland ]; then
        swaymsg -t get_tree | grep -q '"app_id": "dev.zed.Zed"'
    else
        xdotool search --onlyvisible --class dev.zed.Zed >/dev/null
    fi
    ;;
*)
    echo "unknown command: $0" >&2
    exit 2
    ;;
esac
