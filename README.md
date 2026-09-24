# cull

A fast, keyboard-driven tool for culling RAW+JPEG pairs before import. Point it at a folder of shots.
It shows each JPEG full screen and lets you mark shots for deletion. When you finish, deleted shots
(JPEG, RAW and any `.xmp` sidecars) go to the system Trash, and cull offers to move everything left
into a new folder named after the day the last picture was taken.

Works on macOS and Linux (X11/Wayland). Built in Rust with winit, wgpu, zune-jpeg and glyphon.

## Install

```sh
cargo install --path .
```

## Usage

```sh
cull ~/Pictures/2026-09-trip        # full screen
cull --windowed ~/Pictures/2026-09-trip
cull --dest-root ~/Pictures/Library ~/Pictures/2026-09-trip
```

Files are paired by basename (`DSC0001.JPG` + `DSC0001.ARW`), with extensions matched case-insensitively.
Most RAW formats are recognised (ARW, CR2, CR3, NEF, RAF, ORF, RW2, DNG, PEF, …). A RAW with no JPEG
can't be shown, so cull never touches it. A JPEG with no RAW is shown and deleted on its own.

The metadata panel (`m`) includes the in-camera colour profile when the camera records one:
Fujifilm film simulations, Canon Picture Styles (including the base of User Def. styles), Panasonic Film
Mode / Photo Style, Nikon Picture Controls and Sony Creative Styles.

After the Trash step (or straight away, if nothing was marked) the move screen suggests
`<dest-root>/YYYY-MM-DD`. The date is the latest EXIF capture date among the kept shots, or the latest
file modification time if they have none. If that folder exists, cull uses `YYYY-MM-DD_1`, `_2`, …
instead. You can edit the path before confirming, with vi-style keys (it starts in normal mode). The
same rule applies to whatever you type, so nothing is ever overwritten. Every visible file left in the folder is moved (videos, RAWs with no JPEG
and so on); hidden files stay. Files are renamed on the same disk. Across disks they are copied,
synced and only then deleted, with progress shown. `--dest-root` defaults to
`~/mnt/truenas/Pictures/Digital Photography/Raw Shots`, falling back to the folder containing DIR if
that doesn't exist.

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
| Move (normal) | h l, w b, 0 $ | cursor left / right, next / previous word, start / end |
| Move (normal) | i a / I A | insert before / after the cursor, at the start / end |
| Move (normal) | cw | change word |
| Move (normal) | x | delete the character under the cursor |
| Move (insert) | type, Backspace | edit the path (`~` is expanded); Esc back to normal mode |
| Move | Enter | move everything left into it and exit |
| Move (normal) | Esc | leave the files where they are and exit (while moving: stop after the current file) |
| Any | ? | help (not on the move screen) |
