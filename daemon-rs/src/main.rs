//! MagicMouse daemon 2.0 (Rust port of bin/magic-mouse-daemon v1.4.1). See PORT.md.
#![allow(dead_code)]

mod battery;
mod bluez;
mod config;
mod curve;
mod devices;
mod engine;
mod hypr;
mod iface;
mod probe;
mod replay;
mod status;
mod touch;
mod vpad;

fn main() {
    eprintln!("magic-mouse-daemon 2.0 (port in progress)");
}
