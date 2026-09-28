//! NyaRemoteControl host — `nya-server-svc.exe` (design doc §1.1):
//!
//! * `service`    – Windows service (Session 0, SYSTEM): network, sessions, helper management, control pipe
//! * `helper`     – runs in the active console session with a SYSTEM token: capture, encode, input
//! * `standalone` – single user-mode process for development (no lock screen / UAC support)
//! * `diag`       – hardware / encoder diagnostics
//!
//! It is managed by `nya-server.exe` (`manager/`) through the control pipe
//! (`control`); what both sides share lives in `nya-server-core` (`core/`),
//! whose modules are re-exported here.

pub use nya_server_core::{attach_parent_console, auth, components, config, control_pb, fatal, install, logging, paths};

pub mod abr;
pub mod control;
pub mod diag;
pub mod host;
pub mod hub;
pub mod ipc;
pub mod net;
pub mod service;
pub mod state;
pub mod usb;
pub mod winutil;

pub mod ipc_pb {
    include!(concat!(env!("OUT_DIR"), "/nya.ipc.rs"));
}
