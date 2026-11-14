#!/bin/bash
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.

# Builds and runs cfview. Requirements: Rust (any recent stable), the SDL2
# runtime (libSDL2-2.0.so.0), adb, and a running Cuttlefish group with display
# overlays configured. No C compiler, no guest-side component: input is
# injected by writing to the guests' boot-time /dev/input nodes.
#
#   ./run.sh          view display 0
#   ./run.sh 1        view display 1
#   ./run.sh 0 2      view display 0 with vm 2 as the base
#
# Everything else is discovered at runtime from the group's own
# cuttlefish_config.json: which layers to merge, the shm ring names, and each
# VM's adb address. Overrides, if you need them: the ADB and VM<n>_SERIAL env
# vars or a cfview.json (see README.md).
set -e
HERE="$(cd "$(dirname "$0")" && pwd)"

# cfview is a plain host crate. Soong never builds it, so there is no need for
# a system rustup install: borrow the Rust toolchain the AOSP tree already
# ships. Only reach for it if the environment does not already provide a cargo
# on PATH.
if ! command -v cargo >/dev/null; then
  # Walk up from this directory to the first one that holds the tree's
  # prebuilt Rust toolchains.
  DIR="$HERE"
  TOOLCHAINS=""
  while true; do
    if [ -d "$DIR/prebuilts/rust-toolchain/linux-x86" ]; then
      TOOLCHAINS="$DIR/prebuilts/rust-toolchain/linux-x86"
      break
    fi
    if [ "$DIR" = "/" ]; then
      break
    fi
    DIR="$(dirname "$DIR")"
  done
  if [ -n "$TOOLCHAINS" ]; then
    # Newest native linux-x86 toolchain. linux-musl-x86 and linux-arm64 have
    # the wrong libc or arch for a host SDL2 binary. sort -V picks the highest
    # version directory.
    RUST_BIN="$(ls -d "$TOOLCHAINS"/*/bin 2>/dev/null | sort -V | tail -1)"
    if [ -n "$RUST_BIN" ]; then
      export PATH="$RUST_BIN:$PATH"
    fi
  fi
fi
command -v cargo >/dev/null || { echo "ERROR: cargo not found. Install Rust (https://rustup.rs) or keep cfview inside an AOSP tree with prebuilts/rust-toolchain/linux-x86/*/bin"; exit 1; }

# Local target dir: keeps the build self-contained even when this checkout
# lives inside a bigger workspace that redirects cargo's target-dir.
export CARGO_TARGET_DIR="$HERE/target"
cargo build --release --manifest-path "$HERE/Cargo.toml"

exec "$CARGO_TARGET_DIR/release/cfview" "$@"
