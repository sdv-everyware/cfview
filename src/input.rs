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

//! Input delivery to the guests WITHOUT a guest component: SDL event →
//! 24-byte `struct input_event` → adb shell pipe → written straight into
//! /dev/input/eventN.
//!
//! This is the same path the browser uses. The streamer writes events into the
//! virtio-input devices created at kernel boot, and we write into the very same
//! nodes. The key consequence: the device exists BEFORE the consumer starts (an
//! app such as harry scans /dev/input only once), so our input is visible to it
//! at once and for good. No restarts, no uinput, not one line of code in the
//! guest. `getevent -pi` names the nodes for us. For each one we open a
//! long-lived `adb shell 'cat > /dev/input/eventN'` and pour the bytes into it.

use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// SDL scancode (USB HID) → Linux evdev keycode.
#[rustfmt::skip]
const KEYMAP: &[(i32, u16)] = &[
    (4, 30),  (5, 48),  (6, 46),  (7, 32),  (8, 18),  (9, 33),   // A..F
    (10, 34), (11, 35), (12, 23), (13, 36), (14, 37), (15, 38),  // G..L
    (16, 50), (17, 49), (18, 24), (19, 25), (20, 16), (21, 19),  // M..R
    (22, 31), (23, 20), (24, 22), (25, 47), (26, 17), (27, 45),  // S..X
    (28, 21), (29, 44),                                          // Y, Z
    (30, 2),  (31, 3),  (32, 4),  (33, 5),  (34, 6),             // 1..5
    (35, 7),  (36, 8),  (37, 9),  (38, 10), (39, 11),            // 6..0
    (40, 28), /*ENTER*/ (41, 1),  /*ESC*/   (42, 14), /*BKSP*/
    (43, 15), /*TAB*/   (44, 57), /*SPACE*/ (45, 12), (46, 13),
    (47, 26), (48, 27), (49, 43), (51, 39), (52, 40), (53, 41),
    (54, 51), (55, 52), (56, 53), (57, 58),
    (58, 59), (59, 60), (60, 61), (61, 62), (62, 63), (63, 64),  // F1..F6
    (64, 65), (65, 66), (66, 67), (67, 68), (68, 87), (69, 88),  // F7..F12
    (79, 106), /*RIGHT*/ (80, 105), /*LEFT*/ (81, 108), /*DOWN*/ (82, 103), /*UP*/
    (224, 29), /*LCTRL*/ (225, 42), /*LSHIFT*/ (226, 56),  /*LALT*/
    (228, 97), /*RCTRL*/ (229, 54), /*RSHIFT*/ (230, 100), /*RALT*/
];

pub const SDL_SCANCODE_F9: i32 = 66;

pub fn sdl_to_evdev(scancode: i32) -> Option<u16> {
    KEYMAP.iter().find(|(s, _)| *s == scancode).map(|(_, e)| *e)
}

// evdev constants. Touch uses the multitouch type-B protocol: exactly the
// sequence that made a touch button of a guest app respond in a manual test.
const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_ABS: u16 = 3;
const ABS_MT_TRACKING_ID: u16 = 0x39;
const ABS_MT_POSITION_X: u16 = 0x35;
const ABS_MT_POSITION_Y: u16 = 0x36;
const BTN_TOUCH: u16 = 0x14a;

/// A long-lived pipe into one /dev/input/eventN node of one guest.
///
/// `adb shell 'cat > NODE'` keeps the connection open until we close stdin.
/// Events are written as a stream, with no fork per event (unlike `sendevent`,
/// which spawns a process per event). The nodes are 0660 root:input, and shell
/// is in the input group, so root is not needed.
struct Pipe {
    child: Option<Child>,
    label: String,
    // Reopen parameters: adb root/unroot restarts adbd and kills our cat
    // process. This used to disable the pipe for good, and the viewer had to
    // be restarted after every adb root. Now the pipe revives itself: the
    // /dev/input/eventN nodes live until the VM reboots, so a fresh
    // `adb shell cat > NODE` brings the path back at once.
    adb: String,
    serial: String,
    node: String,
    /// Reconnect cooldown, so a dead adb does not cause a storm of forks.
    last_attempt: Instant,
}

impl Pipe {
    fn spawn(adb: &str, serial: &str, node: &str) -> Option<Child> {
        Command::new(adb)
            .args(["-s", serial, "shell", "cat", ">", node])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()
    }

    fn open(adb: &str, serial: &str, node: &str, label: String) -> Pipe {
        let child = Self::spawn(adb, serial, node);
        match &child {
            Some(_) => println!("cfview: input {label} -> {serial}:{node}"),
            None => eprintln!("cfview: input {label} pipe to {serial}:{node} failed"),
        }
        Pipe {
            child,
            label,
            adb: adb.to_string(),
            serial: serial.to_string(),
            node: node.to_string(),
            last_attempt: Instant::now(),
        }
    }

    /// Keeps the pipe alive: notices a dead child (try_wait) and reopens it,
    /// at most once a second.
    fn ensure_alive(&mut self) {
        if let Some(c) = self.child.as_mut() {
            match c.try_wait() {
                Ok(None) => return, // alive
                _ => {
                    // It exited: adbd was restarted (adb root) or the connection dropped.
                    let _ = self.child.take();
                    eprintln!("cfview: input pipe {} died, reconnecting", self.label);
                }
            }
        }
        if self.last_attempt.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.last_attempt = Instant::now();
        self.child = Self::spawn(&self.adb, &self.serial, &self.node);
        if self.child.is_some() {
            println!(
                "cfview: input {} reconnected -> {}:{}",
                self.label, self.serial, self.node
            );
        }
    }

    /// One 24-byte `struct input_event` (16-byte timeval + type + code +
    /// value). The kernel stamps the time itself, so timeval is zero.
    fn emit(&mut self, ev_type: u16, code: u16, value: i32) {
        self.ensure_alive();
        let Some(stdin) = self.child.as_mut().and_then(|c| c.stdin.as_mut()) else { return };
        let mut buf = [0u8; 24];
        buf[16..18].copy_from_slice(&ev_type.to_le_bytes());
        buf[18..20].copy_from_slice(&code.to_le_bytes());
        buf[20..24].copy_from_slice(&value.to_le_bytes());
        if stdin.write_all(&buf).is_err() {
            // EPIPE: the child died between try_wait and write. Bury it and
            // right away try once to reopen and write again. This way the first
            // key press after adb root arrives instead of getting lost.
            eprintln!("cfview: input pipe {} broke mid-write, reconnecting", self.label);
            if let Some(mut c) = self.child.take() {
                let _ = c.kill();
                let _ = c.wait();
            }
            self.last_attempt = Instant::now() - Duration::from_secs(2); // skip the cooldown
            self.ensure_alive();
            if let Some(stdin) = self.child.as_mut().and_then(|c| c.stdin.as_mut()) {
                let _ = stdin.write_all(&buf);
            }
        }
    }

    fn flush(&mut self) {
        if let Some(stdin) = self.child.as_mut().and_then(|c| c.stdin.as_mut()) {
            let _ = stdin.flush();
        }
    }
}

impl Drop for Pipe {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            drop(c.stdin.take());
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// Input channels of one guest: the touch node and the keyboard node (either may be missing).
pub struct Bridge {
    touch: Option<Pipe>,
    keyboard: Option<Pipe>,
}

impl Bridge {
    /// Finds the device nodes with `getevent -pi` and opens pipes to them.
    /// Keyboard = has KEY_ENTER; touch = has BTN_TOUCH or ABS_MT. These are the
    /// same criteria harry uses (`har-user-input-evdev`), so we address exactly
    /// the devices it listens to.
    ///
    /// `display` picks the RIGHT touchscreen: each display has its own
    /// ("Multitouch Touchscreen 0/1/2..."), and touch in the window of display N
    /// must go to touchscreen N, or a click on a cluster display ends up on the
    /// main screen. There is one keyboard per VM.
    pub fn open(adb: &str, serial: &str, display: u32, label: &'static str) -> Bridge {
        ensure_root(adb, serial);
        let (touch_node, kbd_node) = discover_nodes(adb, serial, display);
        let touch = touch_node.map(|n| Pipe::open(adb, serial, &n, format!("{label}/touch")));
        let keyboard = kbd_node.map(|n| Pipe::open(adb, serial, &n, format!("{label}/keys")));
        if touch.is_none() && keyboard.is_none() {
            eprintln!("cfview: no input device nodes found on {serial} -- input to {label} disabled");
        }
        Bridge { touch, keyboard }
    }

    /// Key press or release into the keyboard node.
    pub fn key(&mut self, code: u16, down: bool) {
        let Some(p) = self.keyboard.as_mut() else { return };
        p.emit(EV_KEY, code, down as i32);
        p.emit(EV_SYN, 0, 0);
        p.flush();
    }

    /// Touch down at the display point (x, y). Opens "finger" #0.
    pub fn touch_down(&mut self, x: i32, y: i32) {
        let Some(p) = self.touch.as_mut() else { return };
        p.emit(EV_ABS, ABS_MT_TRACKING_ID, 0);
        p.emit(EV_ABS, ABS_MT_POSITION_X, x);
        p.emit(EV_ABS, ABS_MT_POSITION_Y, y);
        p.emit(EV_KEY, BTN_TOUCH, 1);
        p.emit(EV_SYN, 0, 0);
        p.flush();
    }

    /// Motion of the pressed finger.
    pub fn touch_move(&mut self, x: i32, y: i32) {
        let Some(p) = self.touch.as_mut() else { return };
        p.emit(EV_ABS, ABS_MT_POSITION_X, x);
        p.emit(EV_ABS, ABS_MT_POSITION_Y, y);
        p.emit(EV_SYN, 0, 0);
        p.flush();
    }

    /// Release: tracking id = -1 lifts the finger.
    pub fn touch_up(&mut self) {
        let Some(p) = self.touch.as_mut() else { return };
        p.emit(EV_ABS, ABS_MT_TRACKING_ID, -1);
        p.emit(EV_KEY, BTN_TOUCH, 0);
        p.emit(EV_SYN, 0, 0);
        p.flush();
    }
}

/// One device from the output of `getevent -pi`.
struct DevInfo {
    node: String,
    name: String,
    is_touch: bool,
    is_keyboard: bool,
}

/// Runs one adb command for `serial` and returns its stdout, or None when adb
/// cannot be started, fails, or takes longer than `limit` (then it is killed).
fn adb_run(adb: &str, serial: &str, args: &[&str], limit: Duration) -> Option<String> {
    use std::io::Read;
    let mut child = Command::new(adb)
        .args(["-s", serial])
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut out = String::new();
                child.stdout.take()?.read_to_string(&mut out).ok()?;
                return status.success().then_some(out);
            }
            Ok(None) if start.elapsed() < limit => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

fn is_root(adb: &str, serial: &str) -> Option<bool> {
    adb_run(adb, serial, &["shell", "id", "-u"], Duration::from_secs(5)).map(|o| o.trim() == "0")
}

/// Makes adbd of `serial` run as root, because a guest with SELinux enforcing
/// refuses writes to its input nodes from the adb shell user (the nodes are
/// 0660 root:input and the shell user is in that group, but the policy has no
/// rule for it). `adb root` works on userdebug and eng builds. When it does
/// not, the user is told what that means; the picture needs no adb at all.
fn ensure_root(adb: &str, serial: &str) {
    match is_root(adb, serial) {
        Some(true) => return,
        Some(false) => {}
        None => {
            eprintln!("cfview: {serial}: adb gives no answer ({adb}) -- view only, no input. Is adb installed and the device online? ADB=/path/to/adb picks another adb.");
            return;
        }
    }
    println!("cfview: {serial}: adb root");
    let _ = adb_run(adb, serial, &["root"], Duration::from_secs(10));
    // adbd restarts: the device is gone for a moment
    let _ = adb_run(adb, serial, &["wait-for-device"], Duration::from_secs(15));
    if is_root(adb, serial) == Some(true) {
        return;
    }
    let enforcing = adb_run(adb, serial, &["shell", "getenforce"], Duration::from_secs(5))
        .map(|o| o.trim().eq_ignore_ascii_case("enforcing"));
    if enforcing == Some(false) {
        eprintln!("cfview: {serial}: adb root did not work; SELinux is permissive, so input goes through the adb shell user");
    } else {
        eprintln!("cfview: {serial}: adb root did not work (a user build?) and SELinux is enforcing: the guest refuses mouse and keys from the adb shell user. View only. Input needs a userdebug or eng image, or a guest started permissive.");
    }
}

/// Runs `getevent -pi` and returns its startup dump, killing it after a short
/// window (it never exits on its own, see discover_nodes). Host-side kill so
/// we do not depend on a working guest `timeout`. Returns None if nothing was
/// captured (dead adb, or a VM with no input devices).
fn capture_getevent(adb: &str, serial: &str) -> Option<String> {
    use std::io::Read;
    let mut child = Command::new(adb)
        .args(["-s", serial, "shell", "getevent", "-pi"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    // getevent dumps all device capabilities immediately; 1.5s is plenty.
    let reader = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stdout.read_to_string(&mut buf); // returns when the pipe closes
        buf
    });
    std::thread::sleep(Duration::from_millis(1500));
    let _ = child.kill(); // closes the pipe -> reader thread unblocks
    let _ = child.wait();
    let text = reader.join().unwrap_or_default();
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Parses `getevent -pi` and returns (touch node for `display`, keyboard).
///
/// The parsing goes SECTION BY SECTION, and that was learned the hard way: hex
/// codes mean different things in different sections. `0035` in the KEY section
/// is the "/" key, and in the ABS section it is ABS_MT_POSITION_X. A naive
/// substring search over the whole record declared a full keyboard a
/// touchscreen. The criteria are the same as harry's (har-user-input-evdev):
///   keyboard = KEY has both 001c (ENTER) and 001e (A);
///   touch    = KEY has 014a (BTN_TOUCH) or ABS has axis 0035.
///
/// Touch is picked BY DISPLAY: each screen has its own touchscreen
/// ("... Touchscreen 0/1/2"), and its name ends with that number. The fallback
/// is the first touch node. There is one keyboard per VM: the first one found.
fn discover_nodes(adb: &str, serial: &str, display: u32) -> (Option<String>, Option<String>) {
    // `getevent -pi` prints every device's capabilities at startup and then
    // BLOCKS reading events. On some images it never returns, so `.output()`
    // (which waits for EOF) would hang the whole viewer before the window even
    // opens. We only need the startup dump: spawn it, read for a beat, kill it,
    // parse what was printed. A VM with no /dev/input nodes just yields nothing.
    let text = match capture_getevent(adb, serial) {
        Some(t) => t,
        None => {
            eprintln!("cfview: {serial}: getevent produced nothing (no input nodes?) -- input disabled for this VM");
            return (None, None);
        }
    };

    #[derive(PartialEq, Clone, Copy)]
    enum Section {
        None,
        Key,
        Abs,
    }

    let mut devices: Vec<DevInfo> = Vec::new();
    let mut cur: Option<DevInfo> = None;
    let mut section = Section::None;
    let mut has_enter = false;
    let mut has_a = false;

    let finish = |cur: &mut Option<DevInfo>, has_enter: &mut bool, has_a: &mut bool,
                      devices: &mut Vec<DevInfo>| {
        if let Some(mut d) = cur.take() {
            d.is_keyboard = *has_enter && *has_a;
            devices.push(d);
        }
        *has_enter = false;
        *has_a = false;
    };

    for raw in text.lines() {
        let line = raw.trim();
        if let Some(rest) = line.strip_prefix("add device") {
            finish(&mut cur, &mut has_enter, &mut has_a, &mut devices);
            section = Section::None;
            let node = rest.split_once(':').map(|(_, p)| p.trim().to_string()).unwrap_or_default();
            cur = Some(DevInfo { node, name: String::new(), is_touch: false, is_keyboard: false });
            continue;
        }
        if let Some(rest) = line.strip_prefix("name:") {
            if let Some(d) = cur.as_mut() {
                d.name = rest.trim().trim_matches('"').to_string();
            }
            continue;
        }
        // Capability sections: "KEY (0001): codes...", "ABS (0003): axis : ...".
        // Others ("REL (0002)", "LED", "input props" and so on) reset the section.
        let payload = if let Some(r) = line.strip_prefix("KEY (0001):") {
            section = Section::Key;
            r
        } else if let Some(r) = line.strip_prefix("ABS (0003):") {
            section = Section::Abs;
            r
        } else if line.contains('(') && line.contains("):") {
            section = Section::None;
            continue;
        } else {
            line // continuation line of the current section
        };

        let Some(d) = cur.as_mut() else { continue };
        match section {
            Section::Key => {
                // KEY lines are a list of hex codes separated by spaces.
                for tok in payload.split_whitespace() {
                    match tok {
                        "014a" => d.is_touch = true,
                        "001c" => has_enter = true,
                        "001e" => has_a = true,
                        _ => {}
                    }
                }
            }
            Section::Abs => {
                // ABS lines: "0035 : value 0, min ...", the axis code is the FIRST token.
                if matches!(payload.split_whitespace().next(), Some("0035") | Some("0036")) {
                    d.is_touch = true;
                }
            }
            Section::None => {}
        }
    }
    finish(&mut cur, &mut has_enter, &mut has_a, &mut devices);

    // Touch of the wanted display: its name ends with the display number. Fallback: the first touch.
    let suffix = format!(" {display}");
    let touch = devices
        .iter()
        .find(|d| d.is_touch && d.name.ends_with(&suffix))
        .or_else(|| devices.iter().find(|d| d.is_touch))
        .map(|d| d.node.clone());
    let keyboard = devices.iter().find(|d| d.is_keyboard).map(|d| d.node.clone());
    (touch, keyboard)
}
