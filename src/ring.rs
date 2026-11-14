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

//! Read-only client of cuttlefish's display ring buffers.
//!
//! This is what makes cfview patch-free. Every streamer already publishes each
//! display's frames into a named POSIX shm ring (that is how the overlay
//! compositor of one VM reads another VM's pixels), and the header comment in
//! `host/libs/screen_connector/ring_buffer_manager.h` says the layout exists
//! precisely so that "an external process" can find the current frame. We are
//! that external process.
//!
//! Layout (all confirmed against the running system: the shm files are exactly
//! `16 + 3 * w * h * 4` bytes):
//!
//!     offset 0   u32  display_width
//!     offset 4   u32  display_height
//!     offset 8   u32  bpp                (always 4: RGBA)
//!     offset 12  u32  last_valid_frame_index   (atomic on the writer side)
//!     offset 16  frame[0], frame[1], frame[2]  (ring of 3, tightly packed)
//!
//! Names: `/dev/shm/cf_shmem_display_<vm>_<display>_<group_uuid>`.
//!
//! The coupling to this layout is the price of having no patch in
//! device/google/cuttlefish, and it is a soft price: we only ever map the
//! buffers read-only, so if a future cuttlefish changes the layout, this
//! viewer breaks visibly and cuttlefish does not notice us at all.

use std::fs;
use std::os::fd::AsRawFd;

const HEADER_BYTES: usize = 16;
const RING_FRAMES: u32 = 3;

// The two mmap externs we need. Declared by hand instead of pulling the libc
// crate: the signatures are ABI-stable POSIX, and zero dependencies means the
// build works offline with the pinned tree toolchain.
extern "C" {
    fn mmap(
        addr: *mut u8,
        len: usize,
        prot: i32,
        flags: i32,
        fd: i32,
        offset: i64,
    ) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
}
const PROT_READ: i32 = 1;
const MAP_SHARED: i32 = 1;
const MAP_FAILED: isize = -1;

/// One display's ring, mapped read-only.
pub struct Ring {
    base: *mut u8,
    len: usize,
    pub width: u32,
    pub height: u32,
}

// The raw pointer is to a MAP_SHARED read-only region; nothing about it is
// thread-affine. We only read from the render thread anyway.
unsafe impl Send for Ring {}

impl Ring {
    /// Maps `/dev/shm/<name>`. Plain open+mmap: shm_open() would resolve to
    /// the same file, so we skip the librt detour entirely.
    pub fn open(path: &str) -> Result<Ring, String> {
        let file = fs::File::open(path).map_err(|e| format!("{path}: {e}"))?;
        let len = file
            .metadata()
            .map_err(|e| format!("{path}: {e}"))?
            .len() as usize;
        if len < HEADER_BYTES {
            return Err(format!("{path}: too small for a ring header"));
        }

        // SAFETY: fd is a valid open file, len matches its size, and we drop
        // neither until munmap in Drop. PROT_READ keeps us honest: the kernel
        // enforces that this viewer can never corrupt the streamer's state.
        let base = unsafe {
            mmap(
                std::ptr::null_mut(),
                len,
                PROT_READ,
                MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if base as isize == MAP_FAILED {
            return Err(format!("{path}: mmap failed"));
        }

        // SAFETY: the region is at least HEADER_BYTES long (checked above).
        // Volatile because the writer updates these fields concurrently and we
        // must not let the compiler cache them across the render loop.
        let (width, height, bpp) = unsafe {
            (
                (base as *const u32).read_volatile(),
                (base.add(4) as *const u32).read_volatile(),
                (base.add(8) as *const u32).read_volatile(),
            )
        };
        if bpp != 4 {
            return Err(format!("{path}: unexpected bpp {bpp} (layout changed?)"));
        }
        let expected = HEADER_BYTES + (width * height * 4 * RING_FRAMES) as usize;
        if len < expected {
            return Err(format!(
                "{path}: {len} bytes but {width}x{height} needs {expected} -- layout changed?"
            ));
        }

        Ok(Ring { base, len, width, height })
    }

    /// The writer's frame counter. Changes value = there is a new frame.
    pub fn last_index(&self) -> u32 {
        // SAFETY: offset 12 is inside the mapped header. Volatile: the writer
        // is another process, this is genuinely shared memory.
        unsafe { (self.base.add(12) as *const u32).read_volatile() }
    }

    /// The pixels of ring slot `index`, RGBA8888, tightly packed.
    ///
    /// The writer always writes into `(last+1) % 3` and only then publishes
    /// the new index, so the slot we read stays stable for two full writer
    /// frames. That is more than enough for one texture upload. A tear would need
    /// the writer to lap the whole ring inside our ~1 ms memcpy.
    pub fn frame(&self, index: u32) -> &[u8] {
        let frame_bytes = (self.width * self.height * 4) as usize;
        let offset = HEADER_BYTES + (index % RING_FRAMES) as usize * frame_bytes;
        // SAFETY: offset+frame_bytes <= len was established in open().
        unsafe { std::slice::from_raw_parts(self.base.add(offset), frame_bytes) }
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        // SAFETY: base/len are exactly what mmap returned.
        unsafe { munmap(self.base, self.len) };
    }
}

/// What to composite for one view: the base layer plus overlay layers in
/// paint order. This is exactly what the running group's config says, not a guess.
pub struct View {
    pub base: String,
    /// 0-based index of the base VM in the group (for the adb address and the ring names).
    pub base_vm: u32,
    /// Layers on top, in paint order. The path may not exist yet: a ring
    /// appears only once something starts to render its display.
    pub overlays: Vec<(String, u32)>,
    /// adb addresses of the instances by VM index, from the same config
    /// (adb_ip_and_port). Auto-detection instead of hardcoded ports.
    pub serials: Vec<Option<String>>,
}

/// Reads the freshest group's cuttlefish_config.json and derives which rings
/// to merge for `display`, the same way the streamer's own compositor does.
///
/// This replaces two earlier heuristics at once: the hardcoded "vm0 overlays
/// vm1" convention (the config's per-display `overlays` field is the actual
/// authority: "0:1 " means "vm 0's display 1 goes on top of me"), and the
/// newest-mtime uuid guess (`group_uuid` is stated outright). The JSON file
/// given to `cvd load` is NOT read: it is merely `cvd load`'s input, and the
/// runtime config is what the launched group actually honors.
pub fn find_view(display: u32, force_vm: Option<u32>) -> Result<View, String> {
    // Freshest group dir: /var/tmp/cvd/<uid>/<seq>/. Old ones linger.
    let mut best: Option<(std::time::SystemTime, std::path::PathBuf)> = None;
    let cvd_root = "/var/tmp/cvd";
    for uid_dir in fs::read_dir(cvd_root).map_err(|e| format!("{cvd_root}: {e}"))?.flatten() {
        let Ok(groups) = fs::read_dir(uid_dir.path()) else { continue };
        for group in groups.flatten() {
            let cfg = group.path().join("home/cuttlefish/assembly/cuttlefish_config.json");
            let Ok(meta) = fs::metadata(&cfg) else { continue };
            let Ok(mtime) = meta.modified() else { continue };
            if best.as_ref().is_none_or(|(t, _)| mtime > *t) {
                best = Some((mtime, cfg));
            }
        }
    }
    let (_, cfg_path) = best.ok_or("no cuttlefish_config.json under /var/tmp/cvd -- is Cuttlefish running?")?;
    let text = fs::read_to_string(&cfg_path).map_err(|e| format!("{}: {e}", cfg_path.display()))?;
    let cfg = crate::json::parse(&text)?;

    // group_uuid hides in environments.env-N.group_uuid.
    let uuid = cfg
        .get("environments")
        .and_then(|e| e.as_obj())
        .and_then(|m| m.values().next())
        .and_then(|env| env.get("group_uuid"))
        .and_then(|u| u.as_u64())
        .ok_or("config has no environments.*.group_uuid (layout changed?)")?;

    // Instances in index order: the cf_shmem names use the 0-based index of the
    // VM in the group, and the instances themselves sit in the config under keys
    // like "cvd-1"/"cvd-2". Sort by the instance_dir number.
    let instances = cfg
        .get("instances")
        .and_then(|i| i.as_obj())
        .ok_or("config has no instances")?;
    let mut ordered: Vec<(&String, &crate::json::Value)> = instances.iter().collect();
    // The keys are instance numbers as strings. Sort numerically, so that "10"
    // does not come before "2" once there are more than nine VMs.
    ordered.sort_by_key(|(name, _)| name.parse::<u64>().unwrap_or(u64::MAX));

    // adb addresses of all instances: the input bridges need them. VM index =
    // position in the sorted list (the same index as in the shm ring names).
    let serials: Vec<Option<String>> = ordered
        .iter()
        .map(|(_, inst)| {
            inst.get("adb_ip_and_port").and_then(|s| s.as_str()).map(String::from)
        })
        .collect();

    // The overlays spec of this (vm, display) from the config. Empty in the vast
    // majority of cases: an overlay is the EXCEPTION (for example a safety
    // renderer on top of an infotainment screen), not the rule. Applied
    // dynamically: a spec exists -> composite, no spec -> show the screen as is.
    let overlays_of = |vm: usize| -> String {
        ordered
            .get(vm)
            .and_then(|(_, inst)| inst.get("display_configs"))
            .and_then(|d| d.as_arr())
            .and_then(|arr| arr.get(display as usize))
            .and_then(|d| d.get("overlays"))
            .and_then(|o| o.as_str())
            .unwrap_or("")
            .trim()
            .to_string()
    };

    // Which VM to show as the BASE:
    //   vm argument N -> exactly that VM (for example a VM with its own screen);
    //   otherwise     -> auto: first the VM whose display has something laid over
    //                    it (the base screen of a composite); if there are no
    //                    overlays anywhere, the first VM that renders this display at all.
    let base_vm = if let Some(vm) = force_vm {
        if vm as usize >= ordered.len() {
            return Err(format!("no vm {vm} in the group (only {} instances)", ordered.len()));
        }
        vm as usize
    } else if let Some(idx) = (0..ordered.len()).find(|&i| !overlays_of(i).is_empty()) {
        idx
    } else {
        // no overlays at all: take the first VM that has this display in the config
        (0..ordered.len())
            .find(|&i| {
                ordered[i]
                    .1
                    .get("display_configs")
                    .and_then(|d| d.as_arr())
                    .is_some_and(|a| (display as usize) < a.len())
            })
            .ok_or_else(|| format!("no instance renders display {display}"))?
    };

    let base = format!("/dev/shm/cf_shmem_display_{base_vm}_{display}_{uuid}");
    // "0:0 0:1": vm:display pairs separated by spaces, in paint order. Empty =
    // a standalone screen.
    let overlays: Vec<(String, u32)> = overlays_of(base_vm)
        .split_whitespace()
        .filter_map(|pair| {
            let (vm, disp) = pair.split_once(':')?;
            let vm: u32 = vm.trim().parse().ok()?;
            let disp: u32 = disp.trim().parse().ok()?;
            // A display laid over itself ("0:0" on vm 0, display 0) is there only so that the
            // streamer keeps its rings (it does only with overlays): the base is that layer.
            if vm as usize == base_vm && disp == display {
                return None;
            }
            Some((format!("/dev/shm/cf_shmem_display_{vm}_{disp}_{uuid}"), vm))
        })
        .collect();

    let (name, _) = &ordered[base_vm];
    if overlays.is_empty() {
        println!("cfview: config {} -> {} display {display}, standalone",
                 cfg_path.display(), name);
    } else {
        println!("cfview: config {} -> {} display {display}, overlays '{}'",
                 cfg_path.display(), name, overlays_of(base_vm));
    }
    Ok(View { base, base_vm: base_vm as u32, overlays, serials })
}
