# Zed + fifty_four_lsp + IME(fcitx5/mozc) を仮想ディスプレイ上で動かす E2E 用イメージ。
# ビルドコンテキストはリポジトリルート(lsp/Dockerfile.replay と同じ):
#   docker build -f e2e/Dockerfile -t fifty-four-e2e:latest .
# 起動(仮想ディスプレイ・IME・Zed を立ち上げて待機する):
#   docker run --rm -it -v "$PWD/e2e/out:/out" fifty-four-e2e:latest
# 中で操作するときは e2e/README.md を参照。

ARG ZED_VERSION=v1.26.2

# ---- LSP と拡張(wasm)のビルド -------------------------------------------
# 最終段(下)と同じ ubuntu:24.04 の上で、rustup の Rust を使う。公式の rust:1-bookworm イメージは Debian 12 が元で、
# 既知の CVE が多いため使わない(ビルド段なので出荷物には入らないが、スキャナの一覧には出る。glibc も最終段と揃う)
FROM ubuntu:24.04 AS build
# 依存の C コード(rusqlite の bundled、aws-lc-sys など)と、openssl-sys のビルドに要るもの
# (rust:1-bookworm には最初から入っていた libssl-dev も、ここで明示する)
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential ca-certificates cmake curl git libssl-dev pkg-config \
    && rm -rf /var/lib/apt/lists/*
ENV RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo PATH=/opt/cargo/bin:$PATH
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --profile minimal --default-toolchain stable \
    && rustup target add wasm32-wasip2
WORKDIR /src

# 依存だけ先にビルドしてレイヤーキャッシュを効かせる(lsp/Dockerfile.replay と同じ手口)
COPY Cargo.toml Cargo.lock ./
COPY lsp/Cargo.toml lsp/Cargo.toml
COPY extension/Cargo.toml extension/Cargo.toml
COPY prepare/Cargo.toml prepare/Cargo.toml
RUN mkdir -p lsp/src extension/src prepare/src \
    && echo 'fn main() {}' > lsp/src/main.rs \
    && echo '' > extension/src/lib.rs \
    && echo 'fn main() {}' > prepare/src/main.rs \
    && (cargo build --locked -p fifty_four_lsp || true) \
    && (cargo build --locked -p fifty_four_extension --release --target wasm32-wasip2 || true)

COPY lsp ./lsp
COPY extension ./extension
COPY prepare ./prepare
COPY data ./data
# E2E では FlightRecorder と stderr ミラーが欲しいので debug ビルドにする
RUN touch lsp/src/main.rs extension/src/lib.rs \
    && cargo build --locked -p fifty_four_lsp \
    && cargo build --locked -p fifty_four_extension --release --target wasm32-wasip2

# ---- Zed 本体 -------------------------------------------------------------
FROM ubuntu:24.04 AS zed
ARG ZED_VERSION
ARG TARGETARCH
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
RUN case "$TARGETARCH" in \
        amd64|"") arch=x86_64 ;; \
        arm64) arch=aarch64 ;; \
        *) echo "unsupported arch: $TARGETARCH" >&2; exit 1 ;; \
    esac \
    && curl -fsSL -o /tmp/zed.tar.gz \
        "https://github.com/zed-industries/zed/releases/download/${ZED_VERSION}/zed-linux-${arch}.tar.gz" \
    && mkdir -p /opt \
    && tar -xzf /tmp/zed.tar.gz -C /opt \
    && rm /tmp/zed.tar.gz

# ---- 実行環境 -------------------------------------------------------------
FROM ubuntu:24.04
# - xvfb / x11-utils / xdotool / imagemagick: X11 の仮想ディスプレイ・キー入力・スクリーンショット
# - sway / wtype / grim: Wayland の仮想ディスプレイ(headless)・キー入力・スクリーンショット。
#   Zed の IME は text-input-v3 なので、それを input-method-v2 につなぐ wlroots 系のコンポジタが要る
# - mesa-vulkan-drivers: lavapipe(CPU 実装の Vulkan)。Zed の描画に Vulkan が必須
# - fcitx5 / fcitx5-mozc: 日本語 IME。Zed とは X11 では XIM、Wayland では text-input-v3 でつながる
# - fonts-noto-cjk: 画面比較のため、フォントはイメージで固定する
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        dbus \
        xvfb x11-utils x11-xserver-utils xdotool imagemagick \
        sway wtype grim \
        libvulkan1 mesa-vulkan-drivers \
        libasound2t64 libxkbcommon-x11-0 libxcb1 libx11-xcb1 libfontconfig1 libwayland-client0 libgl1 \
        fcitx5 fcitx5-mozc fcitx5-frontend-gtk3 \
        fonts-noto-cjk \
        git procps \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 10001 --create-home --shell /bin/bash e2e

COPY --from=zed /opt/zed.app /opt/zed.app
RUN ln -s /opt/zed.app/bin/zed /usr/local/bin/zed
COPY --from=build /src/target/debug/fifty_four_lsp /usr/local/bin/fifty_four_lsp
# debug ビルドの LSP は、実行ファイルの隣の db/ に FlightRecorder の DB を作る。e2e ユーザーが書けるようにしておく
RUN mkdir -p /usr/local/bin/db && chown e2e /usr/local/bin/db

# 拡張は「インストール済み」として配置する(Zed は起動時に installed/ を走査して索引を作る)
COPY --chown=e2e extension/extension.toml /home/e2e/.local/share/zed/extensions/installed/fifty-four/extension.toml
COPY --chown=e2e extension/languages /home/e2e/.local/share/zed/extensions/installed/fifty-four/languages
COPY --chown=e2e --from=build /src/target/wasm32-wasip2/release/fifty_four_extension.wasm \
    /home/e2e/.local/share/zed/extensions/installed/fifty-four/extension.wasm

COPY --chown=e2e e2e/docker/zed-settings.json /home/e2e/.config/zed/settings.json
COPY --chown=e2e e2e/docker/fcitx5-profile /home/e2e/.config/fcitx5/profile
COPY --chown=e2e e2e/docker/fcitx5-config /home/e2e/.config/fcitx5/config
COPY --chown=e2e e2e/docker/workspace /home/e2e/workspace
COPY --chown=e2e e2e/docker/sway-config.in /home/e2e/.config/sway/config.in
COPY e2e/docker/entrypoint.sh /usr/local/bin/e2e-entrypoint
COPY e2e/docker/e2e-tools.sh /usr/local/bin/e2e-tools
RUN for c in e2e-key e2e-type e2e-shot e2e-ime e2e-window; do ln -s e2e-tools "/usr/local/bin/$c"; done

USER e2e
WORKDIR /home/e2e
# E2E_DISPLAY: x11 | wayland(entrypoint.sh 参照)
ENV E2E_DISPLAY=x11 \
    DISPLAY=:99 \
    XDG_RUNTIME_DIR=/tmp/e2e-runtime \
    DBUS_SESSION_BUS_ADDRESS=unix:path=/tmp/e2e-dbus \
    LANG=C.UTF-8 \
    LC_CTYPE=C.UTF-8 \
    XMODIFIERS=@im=fcitx \
    GTK_IM_MODULE=fcitx \
    QT_IM_MODULE=fcitx \
    ZED_ALLOW_EMULATED_GPU=1 \
    E2E_SCREEN=1600x1000x24 \
    E2E_OUT=/out
ENTRYPOINT ["/usr/local/bin/e2e-entrypoint"]
CMD ["sleep", "infinity"]
