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

//! SDL2 through dlopen: window, GPU textures and events without a single dev package.
//!
//! The host has the runtime library `libSDL2-2.0.so.0` (ffplay pulls it in), but
//! no headers and no `.so` symlink for the linker. So we link not at build time
//! but at run time: dlopen + dlsym + transmute into typed function pointers.
//! The SDL2 ABI is frozen, so this is more reliable than it sounds.

use std::ffi::{c_char, c_int, c_void, CString};

extern "C" {
    fn dlopen(filename: *const c_char, flags: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}
const RTLD_NOW: c_int = 2;

// Opaque SDL handles: we do not need to know their layout.
pub enum Window {}
pub enum Renderer {}
pub enum Texture {}

/// SDL_Rect: four ints, ABI-stable. Needed for the RenderCopy dst rects and for
/// filling the button strip.
#[repr(C)]
pub struct Rect {
    pub x: c_int,
    pub y: c_int,
    pub w: c_int,
    pub h: c_int,
}

pub const INIT_VIDEO: u32 = 0x20;
pub const WINDOWPOS_CENTERED: c_int = 0x2FFF0000u32 as c_int;
pub const WINDOW_RESIZABLE: u32 = 0x20;
pub const RENDERER_ACCELERATED: u32 = 0x2;
pub const RENDERER_PRESENTVSYNC: u32 = 0x4;
pub const TEXTUREACCESS_STREAMING: c_int = 1;
pub const BLENDMODE_BLEND: c_int = 1;

/// SDL_PIXELFORMAT_ABGR8888, built with the SDL_DEFINE_PIXELFORMAT formula
/// (TYPE_PACKED32=6, ORDER_ABGR=7, LAYOUT_8888=6, 32 bits, 4 bytes).
/// In memory these are the bytes R,G,B,A: exactly what the ring buffers hold.
pub const PIXELFORMAT_ABGR8888: u32 =
    (1 << 28) | (6 << 24) | (7 << 20) | (6 << 16) | (32 << 8) | 4;

pub const QUIT: u32 = 0x100;
pub const KEYDOWN: u32 = 0x300;
pub const KEYUP: u32 = 0x301;
pub const MOUSEMOTION: u32 = 0x400;
pub const MOUSEBUTTONDOWN: u32 = 0x401;
pub const MOUSEBUTTONUP: u32 = 0x402;

/// SDL_Event is a 56-byte union. We declare it as a raw buffer and read the
/// fields we need at fixed offsets of the stable ABI (see SDL_events.h).
#[repr(C)]
pub struct Event(pub [u8; 56]);

impl Event {
    pub fn new() -> Event {
        Event([0; 56])
    }
    pub fn kind(&self) -> u32 {
        u32::from_ne_bytes(self.0[0..4].try_into().unwrap())
    }
    pub fn key_scancode(&self) -> i32 {
        i32::from_ne_bytes(self.0[16..20].try_into().unwrap())
    }
    pub fn key_repeat(&self) -> u8 {
        self.0[13]
    }
    pub fn button(&self) -> u8 {
        self.0[16]
    }
    pub fn motion_state(&self) -> u32 {
        u32::from_ne_bytes(self.0[16..20].try_into().unwrap())
    }
    /// x/y sit at the same offsets in both button and motion events.
    pub fn xy(&self) -> (i32, i32) {
        (
            i32::from_ne_bytes(self.0[20..24].try_into().unwrap()),
            i32::from_ne_bytes(self.0[24..28].try_into().unwrap()),
        )
    }
}

/// The loaded library: typed pointers to everything we need.
#[allow(non_snake_case)]
pub struct Sdl {
    pub Init: unsafe extern "C" fn(u32) -> c_int,
    pub SetHint: unsafe extern "C" fn(*const c_char, *const c_char) -> c_int,
    pub CreateWindow:
        unsafe extern "C" fn(*const c_char, c_int, c_int, c_int, c_int, u32) -> *mut Window,
    pub CreateRenderer: unsafe extern "C" fn(*mut Window, c_int, u32) -> *mut Renderer,
    pub CreateTexture:
        unsafe extern "C" fn(*mut Renderer, u32, c_int, c_int, c_int) -> *mut Texture,
    pub SetTextureBlendMode: unsafe extern "C" fn(*mut Texture, c_int) -> c_int,
    pub UpdateTexture:
        unsafe extern "C" fn(*mut Texture, *const c_void, *const c_void, c_int) -> c_int,
    pub RenderClear: unsafe extern "C" fn(*mut Renderer) -> c_int,
    pub SetRenderDrawColor: unsafe extern "C" fn(*mut Renderer, u8, u8, u8, u8) -> c_int,
    pub RenderFillRect: unsafe extern "C" fn(*mut Renderer, *const Rect) -> c_int,
    pub RenderCopy:
        unsafe extern "C" fn(*mut Renderer, *mut Texture, *const c_void, *const c_void) -> c_int,
    pub RenderPresent: unsafe extern "C" fn(*mut Renderer),
    pub PollEvent: unsafe extern "C" fn(*mut Event) -> c_int,
    pub GetWindowSize: unsafe extern "C" fn(*mut Window, *mut c_int, *mut c_int),
    pub SetWindowTitle: unsafe extern "C" fn(*mut Window, *const c_char),
    pub GetError: unsafe extern "C" fn() -> *const c_char,
}

impl Sdl {
    pub fn load() -> Result<Sdl, String> {
        let name = CString::new("libSDL2-2.0.so.0").unwrap();
        // SAFETY: a plain dlopen; failure is handled.
        let lib = unsafe { dlopen(name.as_ptr(), RTLD_NOW) };
        if lib.is_null() {
            return Err("dlopen libSDL2-2.0.so.0 failed (is SDL2 runtime installed?)".into());
        }

        // Fetches a symbol and transmutes it into the needed function type. A
        // macro, so the name is written once: a copy-paste mistake in a signature
        // here would be a segfault with no diagnostics.
        macro_rules! sym {
            ($name:literal) => {{
                let cname = CString::new($name).unwrap();
                // SAFETY: lib is valid; null is checked; the signature types are
                // the responsibility of the table above, and the SDL2 ABI is stable.
                let ptr = unsafe { dlsym(lib, cname.as_ptr()) };
                if ptr.is_null() {
                    return Err(concat!("dlsym failed: ", $name).into());
                }
                unsafe { std::mem::transmute(ptr) }
            }};
        }

        Ok(Sdl {
            Init: sym!("SDL_Init"),
            SetHint: sym!("SDL_SetHint"),
            CreateWindow: sym!("SDL_CreateWindow"),
            CreateRenderer: sym!("SDL_CreateRenderer"),
            CreateTexture: sym!("SDL_CreateTexture"),
            SetTextureBlendMode: sym!("SDL_SetTextureBlendMode"),
            UpdateTexture: sym!("SDL_UpdateTexture"),
            RenderClear: sym!("SDL_RenderClear"),
            SetRenderDrawColor: sym!("SDL_SetRenderDrawColor"),
            RenderFillRect: sym!("SDL_RenderFillRect"),
            RenderCopy: sym!("SDL_RenderCopy"),
            RenderPresent: sym!("SDL_RenderPresent"),
            PollEvent: sym!("SDL_PollEvent"),
            GetWindowSize: sym!("SDL_GetWindowSize"),
            SetWindowTitle: sym!("SDL_SetWindowTitle"),
            GetError: sym!("SDL_GetError"),
        })
    }

    pub fn error(&self) -> String {
        // SAFETY: SDL_GetError always returns a valid C string.
        unsafe {
            std::ffi::CStr::from_ptr((self.GetError)())
                .to_string_lossy()
                .into_owned()
        }
    }
}
