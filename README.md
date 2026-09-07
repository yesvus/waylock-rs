# waylock-rs

A deliberately small screen locker for Wayland compositors implementing
`ext-session-lock-v1`.

The first scope is intentionally narrow:

- solid-color shared-memory background;
- a built-in bitmap clock;
- a built-in screen-off countdown and optional power-off command;
- PAM authentication through the `swaylock` PAM service;
- no image loading, GPU renderer, GTK, Cairo, Pango, animations, or effects.

The default color matches the current `waybg-rs` setup (`#4B3F72`) and the
default screen-off countdown is 600 seconds after locking.

## Build

```sh
cargo build --release
```

## Usage

```sh
waylock-rs --color 4B3F72 --off-after 600
```

The protocol is compositor-neutral, but a compositor must implement
`ext-session-lock-v1`; Wayland itself does not require every compositor to
provide it. The client must be started inside the graphical session so it
inherits `WAYLAND_DISPLAY`.

The countdown is visual by default. To power off displays when it reaches zero,
provide a compositor-specific command. For Niri, for example:

```sh
waylock-rs --color 4B3F72 --off-after 600 \
  --power-off-command niri msg action power-off-monitors
```

Other compositors can use their own display-power command, or leave this to an
idle daemon such as `swayidle`.

This is an early implementation. It should be tested manually with the real
session before replacing a known-good locker in an idle command.
