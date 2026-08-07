// SPDX-License-Identifier: GPL-3.0-or-later

//! Build script.
//!
//! `option_env!` reads an environment variable at compile time, but Cargo does
//! not know that on its own. Without the line below, a cached build keeps the
//! key it was first compiled with, and a release build could ship the
//! development key. Declaring the dependency makes Cargo rebuild whenever the
//! value changes.

fn main() {
    println!("cargo:rerun-if-env-changed=BRAINMAKER_CONFIG_KEY");
}
