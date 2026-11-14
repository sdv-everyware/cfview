// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! cfview: a standalone Cuttlefish viewer with video and input, and no patches.
//!
//! It needs no patch in device/google/cuttlefish. Frames come straight from the
//! shm ring buffers that the streamers already publish for cross-process overlay
//! composition (see src/ring.rs), and the GPU blends them: the base VM is the
//! bottom layer, and each overlay VM (RGBA with a transparent hole) goes on top
//! through the sampler's alpha blending. What the streamer does on the CPU 50
//! times a second costs nothing here.
//!
//!     /dev/shm/cf_shmem_display_<base vm>_<display>_<uuid>  ──►  base texture
//!     /dev/shm/cf_shmem_display_<overlay vm>_<disp>_<uuid>  ──►  overlay texture (BLEND)
//!     mouse/keyboard ──► adb shell cat ──► /dev/input/eventN (see src/input.rs)
//!
//! Usage:  cfview [display] [vm]     (env: ADB, VM<n>_SERIAL; see src/config.rs)

mod config;
mod input;
mod json;
mod ring;
mod sdl;

use input::Bridge;
use std::collections::HashMap;
use std::ffi::CString;

// --- Strip of control buttons (like the control panel of the web client). ---
//
// The web buttons send {"command":"power",...} over the device-control data
// channel, and the streamer turns them into EV_KEY on the guest's virtual
// keyboard (connection_observer.cpp: OnPowerButton -> KEY_POWER and so on). We do
// the same directly, through our own evdev path, into the VM that has the input
// focus (F9). This matches "a web client page is bound to one device".
const STRIP_W: i32 = 56; // strip width in window pixels

/// Where the video goes in a `ww` x `wh` window: into the area left of the button
/// strip, centered, with the aspect ratio of the `w` x `h` display. The rest is black
/// bars. One rectangle serves both drawing and mapping the mouse to touch, or the
/// touch would drift.
fn video_rect(ww: i32, wh: i32, w: u32, h: u32) -> sdl::Rect {
    let (area_w, area_h) = ((ww - STRIP_W).max(1) as i64, wh.max(1) as i64);
    let (w, h) = (w.max(1) as i64, h.max(1) as i64);
    // limited by the width or the height, whichever comes first
    let (vw, vh) = if area_w * h <= area_h * w {
        (area_w, area_w * h / w)
    } else {
        (area_h * w / h, area_h)
    };
    sdl::Rect {
        x: ((area_w - vw) / 2) as i32,
        y: ((area_h - vh) / 2) as i32,
        w: vw.max(1) as i32,
        h: vh.max(1) as i32,
    }
}
const BTN_TOP: i32 = 10; // offset of the first button
const BTN_SLOT_H: i32 = 64; // vertical step between buttons
const BTN_HIT_H: i32 = 48; // clickable height of a slot (below it is a dead zone)
const ICON_K: i32 = 2; // scale of a mask pixel (16x16 -> 32x32)

// Overlay alpha threshold: a pixel at or above it counts as "overlay content"
// (touch -> overlay VM). Below it is a transparent hole (the touch falls through
// to the base VM).
const ALPHA_HIT: u8 = 8;

// Linux evdev key codes (see connection_observer.cpp of the web streamer).
// IMPORTANT: Android HOME = KEY_HOMEPAGE (172), not KEY_HOME (102, that is MOVE_HOME).
const KEY_VOLUMEDOWN: u16 = 114;
const KEY_VOLUMEUP: u16 = 115;
const KEY_POWER: u16 = 116;
const KEY_BACK: u16 = 158;
const KEY_HOMEPAGE: u16 = 172;

// Icons: 16x16 monochrome masks, bit 15 = left column. No fonts and no
// external assets, in the zero-deps spirit of the whole viewer.
const ICON_POWER: [u16; 16] = [
    0x0180, 0x0180, 0x0180, 0x1998, 0x318C, 0x6186, 0x6186, 0xC003,
    0xC003, 0x6006, 0x6006, 0x300C, 0x1C38, 0x07E0, 0x0000, 0x0000,
];
const ICON_VOL_UP: [u16; 16] = [
    0x0000, 0x0000, 0x0000, 0x0600, 0x0E08, 0x1E08, 0x3E08, 0x3E3E,
    0x3E08, 0x1E08, 0x0E08, 0x0600, 0x0000, 0x0000, 0x0000, 0x0000,
];
const ICON_VOL_DOWN: [u16; 16] = [
    0x0000, 0x0000, 0x0000, 0x0600, 0x0E00, 0x1E00, 0x3E00, 0x3E3E,
    0x3E00, 0x1E00, 0x0E00, 0x0600, 0x0000, 0x0000, 0x0000, 0x0000,
];
const ICON_HOME: [u16; 16] = [
    0x0000, 0x0000, 0x0180, 0x03C0, 0x07E0, 0x0FF0, 0x1FF8, 0x3FFC,
    0x1FF8, 0x1FF8, 0x1E78, 0x1E78, 0x1E78, 0x1FF8, 0x0000, 0x0000,
];
const ICON_BACK: [u16; 16] = [
    0x0000, 0x0000, 0x0000, 0x0000, 0x0400, 0x0C00, 0x1FF8, 0x3FF8,
    0x3FF8, 0x1FF8, 0x0C00, 0x0400, 0x0000, 0x0000, 0x0000, 0x0000,
];

/// Strip buttons from top to bottom: (mask, evdev code, name for the log).
const BUTTONS: [(&[u16; 16], u16, &str); 5] = [
    (&ICON_POWER, KEY_POWER, "power"),
    (&ICON_VOL_UP, KEY_VOLUMEUP, "volumeup"),
    (&ICON_VOL_DOWN, KEY_VOLUMEDOWN, "volumedown"),
    (&ICON_HOME, KEY_HOMEPAGE, "home"),
    (&ICON_BACK, KEY_BACK, "back"),
];

/// Draws a 16x16 mask as k x k squares in the renderer's current draw color.
///
/// # Safety
/// `ren` is a live SDL_Renderer, called from the thread that loaded SDL.
unsafe fn draw_icon(s: &sdl::Sdl, ren: *mut sdl::Renderer, mask: &[u16; 16], x0: i32, y0: i32) {
    for (row, bits) in mask.iter().enumerate() {
        for col in 0..16u32 {
            if bits & (0x8000 >> col) != 0 {
                let r = sdl::Rect {
                    x: x0 + col as i32 * ICON_K,
                    y: y0 + row as i32 * ICON_K,
                    w: ICON_K,
                    h: ICON_K,
                };
                (s.RenderFillRect)(ren, &r);
            }
        }
    }
}

fn main() {
    // Arguments: [display] [vm]. `vm` picks the VM whose display is the base
    // (for example a VM with its own screen, shown on its own). Without it, the
    // usual auto mode follows the overlays from the config.
    let display: u32 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let force_vm: Option<u32> = std::env::args().nth(2).and_then(|s| s.parse().ok());

    // --- Composition plan from the config of the running group: the base plus
    // the overlays in paint order. Exactly the data the streamer's compositor
    // uses, so our picture has to match the browser's. ---
    let view = match ring::find_view(display, force_vm) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("cfview: {e}");
            std::process::exit(1);
        }
    };
    println!("cfview: base    {}", view.base);
    let base = ring::Ring::open(&view.base).unwrap_or_else(|e| {
        // The streamer keeps its rings only when a display of the group has overlays
        // (frontend/webrtc/main.cpp, OverlaysEnabled); one VM can lay a display over itself.
        eprintln!("cfview: no display ring: {e}");
        eprintln!("cfview: Cuttlefish keeps the rings only with display overlays; launch with");
        eprintln!("cfview: e.g. --display0=width=1920,height=1080,overlays=0:0");
        std::process::exit(1);
    });
    // An overlay ring appears only once something renders its display.
    // Missing ones are skipped, and each one is reported.
    // Overlay layers and the VM of each (same order, bottom to top in z). Skipped
    // ones (ring not there yet) do not break the pairing: only successful ones are pushed.
    let mut overlays: Vec<ring::Ring> = Vec::new();
    let mut overlay_vms: Vec<u32> = Vec::new();
    for (path, vm) in view.overlays.iter() {
        match ring::Ring::open(path) {
            Ok(r) => {
                println!("cfview: overlay vm{vm} {path}");
                overlays.push(r);
                overlay_vms.push(*vm);
            }
            Err(e) => println!("cfview: overlay not present yet ({e}) -- skipping"),
        }
    }
    let (w, h) = (base.width, base.height);
    println!("cfview: {w}x{h}, {} overlay layer(s), blending on the GPU", overlays.len());

    // --- Input. adb addresses: override (env/file) > cuttlefish_config.
    // The keyboard targets the VM of the first overlay. Touch targets the base
    // VM unless an overlay pixel takes it (see below). Each guest gets its own
    // input bridge.
    let settings = config::load();
    let overlay_vm = view.overlays.first().map(|(_, vm)| *vm).unwrap_or(view.base_vm);
    let serial_of = |vm: u32| -> String {
        settings
            .serials
            .get(&vm)
            .cloned()
            .or_else(|| view.serials.get(vm as usize).cloned().flatten())
            .unwrap_or_else(|| format!("0.0.0.0:{}", 6520 + vm))
    };
    let adb = settings.adb.clone();
    // HAR_WIDTH no longer drives touch (routing now goes by overlay alpha). The
    // field is still read for config compatibility, but it is not used.
    let _har_width = settings.har_width;

    // One input bridge for EACH VM (the base and the VM of each overlay), by index.
    // vm_order is the order of the F9 cycle (the base first, then the overlays).
    // We write straight into the boot-time virtio nodes over adb. `display` picks
    // the touchscreen of exactly this display (the window of display 1 ->
    // touchscreen 1, not the main one).
    let mut bridges: HashMap<u32, Bridge> = HashMap::new();
    let mut vm_order: Vec<u32> = Vec::new();
    for vm in std::iter::once(view.base_vm).chain(overlay_vms.iter().copied()) {
        if bridges.contains_key(&vm) {
            continue;
        }
        let serial = serial_of(vm);
        let label = if vm == view.base_vm { "base-vm" } else { "overlay-vm" };
        bridges.insert(vm, Bridge::open(&adb, &serial, display, label));
        vm_order.push(vm);
    }
    // The keyboard goes to the first overlay by default. F9 cycles through all VMs.
    let mut kbd_vm = overlay_vm;
    println!(
        "cfview: adb {} | {} VM bridge(s) {:?} | keys -> vm{kbd_vm} (F9 cycles)",
        settings.adb,
        bridges.len(),
        vm_order
    );

    // --- window ---
    let s = sdl::Sdl::load().unwrap_or_else(|e| panic!("{e}"));
    // SAFETY: from here on, this file makes FFI calls into the loaded SDL. The
    // contracts are simple: Init comes before everything else, and the objects
    // are created once and live until exit.
    unsafe {
        // Linear filtering for downscaling, or text at half window size falls apart.
        let hint_name = CString::new("SDL_RENDER_SCALE_QUALITY").unwrap();
        let hint_val = CString::new("1").unwrap();
        (s.SetHint)(hint_name.as_ptr(), hint_val.as_ptr());

        if (s.Init)(sdl::INIT_VIDEO) != 0 {
            panic!("SDL_Init: {}", s.error());
        }

        // The title is built from the ACTUAL composition plan (the group config),
        // not from hardcoded assumptions: which layers we show and which VM the
        // keyboard goes to right now. F9 switches the input target.
        let layers = {
            let mut l = format!("vm{} d{display}", view.base_vm);
            for (_, vm) in view.overlays.iter() {
                l.push_str(&format!(" + vm{vm} overlay"));
            }
            l
        };
        // The title is dynamic: it shows the current keyboard target VM. F9 cycles
        // through all VMs (touch is routed by alpha and has nothing to do with the title).
        let make_title = |kbd_vm: u32| -> CString {
            CString::new(format!("Cuttlefish Viewer [{layers}] | keys: vm{kbd_vm} (F9 cycles)"))
                .unwrap()
        };
        let mut cur_title = make_title(kbd_vm);

        let win = (s.CreateWindow)(
            cur_title.as_ptr(),
            sdl::WINDOWPOS_CENTERED,
            sdl::WINDOWPOS_CENTERED,
            (w / 2) as i32 + STRIP_W, // + the button strip on the right
            (h / 2) as i32,
            sdl::WINDOW_RESIZABLE,
        );
        let ren = (s.CreateRenderer)(
            win,
            -1,
            sdl::RENDERER_ACCELERATED | sdl::RENDERER_PRESENTVSYNC,
        );
        let tex_base = (s.CreateTexture)(
            ren,
            sdl::PIXELFORMAT_ABGR8888,
            sdl::TEXTUREACCESS_STREAMING,
            w as i32,
            h as i32,
        );
        let tex_overlays: Vec<*mut sdl::Texture> = overlays
            .iter()
            .map(|o| {
                let t = (s.CreateTexture)(
                    ren,
                    sdl::PIXELFORMAT_ABGR8888,
                    sdl::TEXTUREACCESS_STREAMING,
                    o.width as i32,
                    o.height as i32,
                );
                // The whole composition is this line: the GPU blends the layer by alpha.
                (s.SetTextureBlendMode)(t, sdl::BLENDMODE_BLEND);
                t
            })
            .collect();
        if win.is_null() || ren.is_null() || tex_base.is_null()
            || tex_overlays.iter().any(|t| t.is_null())
        {
            panic!("SDL setup failed: {}", s.error());
        }

        // --- input state ---
        let mut mouse_down = false;
        let mut gesture_vm = view.base_vm; // target VM of the current gesture (latched on press)
        // Ring indices: 3 = "not loaded yet", outside the range 0..2.
        let mut shown_base = 3u32;
        let mut shown_overlays = vec![3u32; overlays.len()];
        // Pressed strip button: key-down was sent, waiting for the mouse release.
        let mut pressed_btn: Option<usize> = None;

        'outer: loop {
            let mut ev = sdl::Event::new();
            while (s.PollEvent)(&mut ev) != 0 {
                match ev.kind() {
                    sdl::QUIT => break 'outer,

                    sdl::KEYDOWN | sdl::KEYUP => {
                        if ev.key_repeat() != 0 {
                            continue; // the guest's evdev does the autorepeat
                        }
                        let down = (ev.kind() == sdl::KEYDOWN) as i32;
                        let sc = ev.key_scancode();
                        if sc == input::SDL_SCANCODE_F9 && down == 1 {
                            // Cycle the keyboard target through all VMs (the base and the overlays).
                            let cur = vm_order.iter().position(|&v| v == kbd_vm).unwrap_or(0);
                            kbd_vm = vm_order[(cur + 1) % vm_order.len()];
                            cur_title = make_title(kbd_vm);
                            (s.SetWindowTitle)(win, cur_title.as_ptr());
                            eprintln!("cfview: keyboard -> vm{kbd_vm}");
                            continue;
                        }
                        let Some(code) = input::sdl_to_evdev(sc) else { continue };
                        if let Some(kbd) = bridges.get_mut(&kbd_vm) {
                            kbd.key(code, down == 1);
                        }
                    }

                    sdl::MOUSEBUTTONDOWN | sdl::MOUSEBUTTONUP | sdl::MOUSEMOTION => {
                        let is_motion = ev.kind() == sdl::MOUSEMOTION;
                        if is_motion {
                            // Drag only: state is a bit mask of the pressed buttons.
                            if ev.motion_state() & 1 == 0 {
                                continue;
                            }
                        } else if ev.button() != 1 {
                            continue; // left button only
                        }
                        // The window scales, so convert to display coordinates.
                        // The video fills the window WITHOUT the button strip on the right.
                        let (mut ww, mut wh) = (0i32, 0i32);
                        (s.GetWindowSize)(win, &mut ww, &mut wh);
                        let (x, y) = ev.xy();
                        let vw = (ww - STRIP_W).max(1);

                        // --- button strip: clicks on it are key events, not touch ---
                        if !is_motion && x >= vw {
                            if ev.kind() == sdl::MOUSEBUTTONDOWN {
                                let rel = y - BTN_TOP;
                                let slot = rel / BTN_SLOT_H;
                                if rel >= 0
                                    && (slot as usize) < BUTTONS.len()
                                    && rel % BTN_SLOT_H < BTN_HIT_H
                                {
                                    let i = slot as usize;
                                    let (_, code, name) = BUTTONS[i];
                                    eprintln!("cfview: button {name} -> vm{kbd_vm}");
                                    pressed_btn = Some(i);
                                    if let Some(kbd) = bridges.get_mut(&kbd_vm) {
                                        kbd.key(code, true);
                                    }
                                }
                            } else if let Some(i) = pressed_btn.take() {
                                // release over the strip
                                if let Some(kbd) = bridges.get_mut(&kbd_vm) {
                                    kbd.key(BUTTONS[i].1, false);
                                }
                            }
                            continue;
                        }
                        // Button release, even if the mouse has left the strip:
                        // the key-up must follow the key-down.
                        if ev.kind() == sdl::MOUSEBUTTONUP {
                            if let Some(i) = pressed_btn.take() {
                                if let Some(kbd) = bridges.get_mut(&kbd_vm) {
                                    kbd.key(BUTTONS[i].1, false);
                                }
                                continue;
                            }
                        }
                        // A drag that started on a button never becomes a touch.
                        if is_motion && pressed_btn.is_some() {
                            continue;
                        }

                        // The video is fitted with its aspect ratio (video_rect). A press on
                        // the black bars is not a touch. Motion and release of a gesture are
                        // clamped to the edge, so the gesture in the guest always ends.
                        let r = video_rect(ww, wh, w, h);
                        let inside = x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h;
                        if ev.kind() == sdl::MOUSEBUTTONDOWN && !inside {
                            continue;
                        }
                        let tx = (((x - r.x) as i64 * w as i64 / r.w as i64) as i32).clamp(0, w as i32 - 1);
                        let ty = (((y - r.y) as i64 * h as i64 / r.h as i64) as i32).clamp(0, h as i32 - 1);

                        match ev.kind() {
                            sdl::MOUSEBUTTONDOWN => {
                                mouse_down = true;
                                // z stack: walk from the TOP overlay (the last one in
                                // paint order) down. The first opaque pixel at the
                                // touch point takes the touch for its VM. If none is
                                // opaque, the touch falls through to the base
                                // (base_vm). A hidden or moved overlay is transparent,
                                // so the touch goes lower by itself.
                                // The target is pinned until release: a drag does not jump.
                                gesture_vm = view.base_vm;
                                for i in (0..overlays.len()).rev() {
                                    let o = &overlays[i];
                                    let ox = (tx as i64 * o.width as i64 / w as i64)
                                        .clamp(0, o.width as i64 - 1)
                                        as usize;
                                    let oy = (ty as i64 * o.height as i64 / h as i64)
                                        .clamp(0, o.height as i64 - 1)
                                        as usize;
                                    // ABGR8888: alpha is the 4th byte of the pixel.
                                    let a = o
                                        .frame(o.last_index())
                                        .get((oy * o.width as usize + ox) * 4 + 3)
                                        .copied()
                                        .unwrap_or(0);
                                    if a >= ALPHA_HIT {
                                        gesture_vm = overlay_vms[i];
                                        break;
                                    }
                                }
                                eprintln!("cfview: touch ({tx},{ty}) -> vm{gesture_vm}");
                                if let Some(t) = bridges.get_mut(&gesture_vm) {
                                    t.touch_down(tx, ty);
                                }
                            }
                            sdl::MOUSEBUTTONUP => {
                                mouse_down = false;
                                if let Some(t) = bridges.get_mut(&gesture_vm) {
                                    t.touch_up();
                                }
                            }
                            _ if mouse_down => {
                                if let Some(t) = bridges.get_mut(&gesture_vm) {
                                    t.touch_move(tx, ty);
                                }
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }

            // Upload only frames that changed: with a static Android screen,
            // last_index stays put and its texture is not touched at all.
            let bi = base.last_index();
            if bi != shown_base {
                shown_base = bi;
                (s.UpdateTexture)(
                    tex_base,
                    std::ptr::null(),
                    base.frame(bi).as_ptr() as *const _,
                    (w * 4) as i32,
                );
            }
            for (i, o) in overlays.iter().enumerate() {
                let oi = o.last_index();
                if oi != shown_overlays[i] {
                    shown_overlays[i] = oi;
                    (s.UpdateTexture)(
                        tex_overlays[i],
                        std::ptr::null(),
                        o.frame(oi).as_ptr() as *const _,
                        (o.width * 4) as i32,
                    );
                }
            }

            // The video fits into the window MINUS the button strip on the right, with
            // the display's aspect ratio (video_rect). The strip stays at the right edge.
            let (mut ww, mut wh) = (0i32, 0i32);
            (s.GetWindowSize)(win, &mut ww, &mut wh);
            let vw = (ww - STRIP_W).max(1);
            let video_dst = video_rect(ww, wh, w, h);

            (s.SetRenderDrawColor)(ren, 0, 0, 0, 255);
            (s.RenderClear)(ren);
            (s.RenderCopy)(
                ren,
                tex_base,
                std::ptr::null(),
                &video_dst as *const sdl::Rect as *const _,
            );
            // Layers in config order, as the streamer's compositor would lay them.
            for t in &tex_overlays {
                (s.RenderCopy)(
                    ren,
                    *t,
                    std::ptr::null(),
                    &video_dst as *const sdl::Rect as *const _,
                );
            }

            // --- button strip ---
            (s.SetRenderDrawColor)(ren, 24, 24, 28, 255);
            let strip = sdl::Rect { x: vw, y: 0, w: STRIP_W, h: wh };
            (s.RenderFillRect)(ren, &strip);
            for (i, (mask, _, _)) in BUTTONS.iter().enumerate() {
                let y0 = BTN_TOP + i as i32 * BTN_SLOT_H;
                if pressed_btn == Some(i) {
                    (s.SetRenderDrawColor)(ren, 70, 70, 80, 255);
                    let hl = sdl::Rect { x: vw + 4, y: y0 - 4, w: STRIP_W - 8, h: BTN_HIT_H };
                    (s.RenderFillRect)(ren, &hl);
                }
                (s.SetRenderDrawColor)(ren, 225, 228, 235, 255);
                draw_icon(&s, ren, mask, vw + (STRIP_W - 16 * ICON_K) / 2, y0 + 4);
            }
            (s.RenderPresent)(ren); // vsync paces the loop
        }
    }
    println!("cfview: bye");
}
