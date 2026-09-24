# cull

A fast, keyboard-driven tool for culling RAW+JPEG pairs before import. Point it at a folder of shots.
It shows each JPEG full screen and lets you mark shots for deletion. When you finish, deleted shots
(JPEG, RAW and any `.xmp` sidecars) go to the system Trash.

Works on macOS and Linux (X11/Wayland). Built in Rust with winit, wgpu, zune-jpeg and glyphon.

## Install

```sh
cargo install --path .
```

## Usage

```sh
cull ~/Pictures/2026-09-trip        # full screen
cull --windowed ~/Pictures/2026-09-trip
```

Files are paired by basename (`DSC0001.JPG` + `DSC0001.ARW`), with extensions matched case-insensitively.
Most RAW formats are recognised (ARW, CR2, CR3, NEF, RAF, ORF, RW2, DNG, PEF, …). A RAW with no JPEG
can't be shown, so cull never touches it. A JPEG with no RAW is shown and deleted on its own.

The metadata panel (`m`) includes the in-camera colour profile when the camera records one:
Fujifilm film simulations, Canon Picture Styles (including the base of User Def. styles), Panasonic Film
Mode / Photo Style, Nikon Picture Controls and Sony Creative Styles.

Marks and your position are saved to `.cull-state.json` in the folder, so you can quit and come back later.

## Keys

| Screen | Key | Action |
|---|---|---|
| Browse | ← → / h l (also ↑ ↓ / k j) | previous / next |
| Browse | Space | mark / unmark for deletion (greys out, red frame) |
| Browse | f, or click | focus peek at 100% (click peeks at that spot) |
| Browse | g / G, Home / End | first / last |
| Browse / Peek | m | show / hide shooting metadata (camera, lens, colour profile, focal length, aperture, shutter, ISO, exposure comp., time) |
| Browse | q / Esc | review marked shots and finish (quits if none are marked) |
| Peek | arrows / hjkl | pan (hold Shift for bigger steps); drag or scroll also pan |
| Peek | Space | mark / unmark for deletion |
| Peek | n / p | next / previous shot, keeping the same pan position |
| Peek | f / Esc | back to full screen |
| Review | arrows / hjkl, scroll | move / scroll |
| Review | Space, or click | keep this shot (clear its mark) |
| Review | y / Enter | move the marked shots to Trash and exit |
| Review | n / Esc | back to browsing |
| Any | ? | help |
