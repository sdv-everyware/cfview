# cfview

cfview is a standalone viewer for Cuttlefish displays. It shows one display of
a running Cuttlefish group in an SDL2 window, lays the overlay layers of other
VMs on top of it on the GPU, and sends mouse and keyboard input back to the
guests. It needs no WebRTC, no browser, no video encoder and no patch in
device/google/cuttlefish.

It is one Rust crate with no external crates: std only, plus SDL2 loaded at
run time with dlopen.

## Frames

The Cuttlefish streamer publishes the frames of each display into a POSIX shm
ring:

```
/dev/shm/cf_shmem_display_<vm>_<display>_<group_uuid>
```

A ring is a 16-byte header (u32 width, u32 height, u32 bpp = 4, u32
last_valid_frame_index) and 3 tightly packed RGBA frames. The writer fills
slot `(last + 1) % 3` and only then publishes the index, so the slot a reader
picks stays stable for two writer frames. cfview maps the rings read-only,
checks the file size against the header, and uploads a frame to a texture
only when the index changes.

Cuttlefish keeps these rings only when a display of the group has overlays.
In a group of several VMs, one VM's display can be laid over another's. A
single VM can lay its display over itself, which keeps the rings and does not
change the picture:

```
--display0=width=1920,height=1080,overlays=0:0
```

`display0` is the VM's first display, `width` and `height` its size (both are
required by Cuttlefish; put the size of your display there), and `overlays=0:0`
is display 0 of VM 0: the display itself.

cfview reads the newest `cuttlefish_config.json` under `/var/tmp/cvd` and
takes from it the `group_uuid`, the instances with their adb addresses, and
the `overlays` of each display (`vm:display` pairs separated by spaces, in
paint order, so `0:1` is display 1 of vm 0). The base layer is the VM given
on the command line, else the first VM whose display has overlays, else the
first VM that renders the display. cfview draws the base texture and then
each overlay with alpha blending, in config order. An overlay ring that does
not exist yet is skipped with a message. A display laid over itself is shown
as the base only.

## Input

Input needs no component in the guest. At startup cfview runs
`adb shell getevent -pi` on the base VM and on the VM of each overlay and
picks the boot-time virtio input nodes:

- keyboard: a device whose KEY codes include both KEY_ENTER and KEY_A
- touchscreen: a device with BTN_TOUCH or the ABS_MT_POSITION_X/Y axes; with
  several touchscreens, the one whose name ends with the display number, else
  the first one

For each node cfview keeps a long-lived `adb shell 'cat > /dev/input/eventN'`
open and writes 24-byte `struct input_event` records into it (multitouch
protocol type B for touch). When a pipe dies, cfview opens it again, at most
once a second.

Writing to the nodes needs adb as root: a guest with SELinux enforcing refuses
the adb shell user, although the nodes are `0660 root:input` and that user is
in the input group. So cfview runs `adb root` at start on every VM that is not
root yet. That works on userdebug and eng images. Where it does not, cfview
says so and shows the picture without input.

Routing:

- The left mouse button acts as one finger. A press goes to the VM of the
  topmost overlay whose pixel under the cursor is not transparent (alpha 8 or
  more), else to the base VM. The target stays the same until the button is
  released.
- The keyboard goes to the VM of the first overlay, or to the base VM when
  there is no overlay. F9 cycles the keyboard target through all VMs. The
  window title shows the current target.
- The strip on the right side of the window has Power, Volume up, Volume
  down, Home and Back. They send key events to the current keyboard target,
  like the buttons of the WebRTC client.

## Requirements

- A Linux host with a running Cuttlefish group that has display overlays (see
  above)
- The SDL2 runtime library `libSDL2-2.0.so.0` (no dev package needed)
- adb, for input: the Cuttlefish host package installs one, and cfview finds
  it. Without adb cfview shows the screen and sends no input
- For input, a userdebug or eng image (cfview runs `adb root`), or a guest
  started with SELinux permissive
- To build it: Rust 1.82 or newer (run.sh can use the prebuilt toolchain of an
  AOSP tree)

## Download

A built binary for x86-64 Linux (glibc 2.28 or newer) is on the
[releases page](https://github.com/sdv-everyware/cfview/releases), with its
SHA-256:

```bash
chmod +x cfview-1.0.1-x86_64-linux
./cfview-1.0.1-x86_64-linux [display] [vm]
```

## Build and run

```bash
./run.sh            # display 0
./run.sh 1          # display 1, for example a cluster display
./run.sh 0 2        # display 0 with vm 2 as the base
```

`./run.sh [display] [vm]` builds a release binary into `./target` and starts
it. `vm` is the 0-based index of the VM in the group, the same index as in the
ring names. If there is no cargo on PATH, run.sh walks up from its own
directory to the first directory that contains
`prebuilts/rust-toolchain/linux-x86` and uses the newest toolchain there.

By hand:

```bash
cargo build --release
./target/release/cfview [display] [vm]
```

## Configuration

Everything is detected at run time. Overrides take this order: environment
variables, then `cfview.json`, then auto-detection.

| Setting | Env | cfview.json | Default |
|---|---|---|---|
| adb binary | `ADB` | `"adb"` | `adb` on PATH, then `~/Android/Sdk/platform-tools/adb`, `/usr/lib/cuttlefish-common/bin/adb`, `/opt/android-sdk/platform-tools/adb` |
| adb address of VM n | `VM<n>_SERIAL`, n = 0..9 | `"serials": { "<n>": "..." }` | `adb_ip_and_port` from `cuttlefish_config.json`, else `0.0.0.0:<6520 + n>` |

cfview reads `cfview.json` from the current directory, then from
`$XDG_CONFIG_HOME/cfview.json`, or from `~/.config/cfview.json` when
`XDG_CONFIG_HOME` is not set:

```json
{
  "adb": "/path/to/adb",
  "serials": { "0": "0.0.0.0:6520", "1": "0.0.0.0:6521" }
}
```

## Notes

- The ring layout is internal to Cuttlefish
  (`host/libs/screen_connector/ring_buffer_manager.h`). cfview maps the rings
  read-only and refuses a file whose size does not match its header, so a
  layout change breaks the viewer visibly and never affects Cuttlefish.
- Old groups leave their shm files behind. cfview always follows the newest
  group config. With Cuttlefish stopped, `rm /dev/shm/cf_shmem_display_*` is
  safe.
- If input does not arrive, look at the first lines of the log:
  `input base-vm/touch -> <serial>:/dev/input/eventN` and
  `input base-vm/keys -> ...` must point at a touchscreen and a keyboard.
  Check with `adb shell getevent -pi | grep -E "add device|name:"`.

## License

Apache License 2.0, see [LICENSE](LICENSE).
