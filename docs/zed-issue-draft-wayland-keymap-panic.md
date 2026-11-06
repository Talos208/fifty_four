# Zed への issue 下書き: Wayland で、keymap より先に `wl_keyboard.modifiers` が届くと `unwrap()` で panic する

E2E テスト環境(`e2e/`、sway の headless バックエンド上で Zed を動かす)で、Zed が起動直後にまれに panic する。
`README` の「調べてわかったこと」に経緯がある。fifty_four 側では回避できない(Zed 本体の問題)ため、upstream へ報告する。

**未提出**(提出したら、ここに URL を書く)。

以下、そのまま GitHub issue として使える形式の下書き。再現の頻度やイベントの順序は、手元の観察に基づく推測を含むので、
本文でもそのように書いてある(確かめられていないことを断定していない)。

---

## Title

Wayland: panic (`Option::unwrap()` on `keymap_state`) when `wl_keyboard.modifiers` arrives before `wl_keyboard.keymap`

## Body

### Description

Zed intermittently panics right after startup on Wayland, in `wl_keyboard::Event::Modifiers` handling:

```
ERROR [crashes] thread 'main' panicked at crates/gpui_linux/src/linux/wayland/client.rs:2124:64:
called `Option::unwrap()` on a `None` value
```

`WaylandClientState::keymap_state` is `None` until the first `wl_keyboard.keymap` event is handled, but the
`Modifiers` handler (and the `Key` handler) call `unwrap()` on it. If a compositor sends `modifiers` (or `key`) before
`keymap`, Zed crashes instead of ignoring the event.

### Where (pinned to the commit of the stable release `1.26.2+stable.384`, `c4273271092b0287f7d181f616bd5171e0a2f70c`)

`crates/gpui_linux/src/linux/wayland/client.rs`

- `keymap_state: Option<xkb::State>` is initialised to `None` (line 1083) and only set in the `Keymap` arm (line 2082).
- The `Modifiers` arm unwraps it (line 2124, the one in the panic message):

  ```rust
  wl_keyboard::Event::Modifiers { mods_depressed, mods_latched, mods_locked, group, .. } => {
      let focused_window = state.keyboard_focused_window.clone();

      let keymap_state = state.keymap_state.as_mut().unwrap();   // <-- panics when no keymap yet
  ```

- The `Key` arm has the same pattern (line 2161: `state.keymap_state.as_ref().unwrap()`).

### Environment and how we hit it

- Zed `1.26.2+stable.384.c427327`, Linux (Ubuntu 24.04 container), software rendering (lavapipe, `ZED_ALLOW_EMULATED_GPU=1`)
- sway 1.9 with the **headless backend** (`WLR_BACKENDS=headless`, no physical input devices, `WLR_LIBINPUT_NO_DEVICES=1`)
- fcitx5 (Wayland input method frontend), which creates a `zwp_virtual_keyboard_v1` on the seat at startup
- Zed is started right after fcitx5 is up, with a workspace and a file as arguments

With no physical keyboard, the seat's keyboard seems to come only from the virtual keyboard. We suspect the keymap is not
yet attached to the seat's keyboard when Zed binds `wl_keyboard`, and that sway/wlroots later sends `modifiers` (e.g. on
`enter`) first. **We have not verified the order of events**: Zed uses wayland-rs, which ignores `WAYLAND_DEBUG`, so we
could not capture the client-side protocol log.

### How often

Intermittent, and it depends on host load:

- roughly 3% to 50% of container starts, depending on how busy the host is (e.g. right after building the image,
  or when several containers start at once); we saw 1/36 on a quiet host and 9/18 on a busy one
- **Persistent within one container**: restarting Zed inside the same container (3 attempts, a few seconds apart) crashed
  every time in the cases where the first start crashed; a fresh container usually starts fine
- Not seen with X11 (Xvfb) in the same setup

### Expected behavior

Zed should not panic. Until the first `keymap` event has been handled, `Modifiers` and `Key` events can be ignored
(or the state can be updated once the keymap arrives).

### Suggested fix

Treat a missing keymap like the other early-return paths in this handler (e.g. the `format != XkbV1` case in the
`Keymap` arm just logs and returns):

```rust
let Some(keymap_state) = state.keymap_state.as_mut() else {
    log::warn!("wl_keyboard.modifiers received before wl_keyboard.keymap; ignoring");
    return;
};
```

and the same for the `Key` arm.

### Related

(none found; if you know of an existing issue about a headless/virtual-keyboard-only seat, please link it)
