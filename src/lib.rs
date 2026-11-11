//! keyboard-follows-mouse core: pure routing logic and the wire protocol.
//! The binaries (`kfm-mac`, `kfm-receive`, `kfm-karabiner`, `kfm-send`) own the sockets, the
//! Karabiner-Elements endpoint and the Windows `SendInput` injector; this
//! library has no OS input hooks and is what the unit tests exercise.

pub mod flow;
pub mod karabiner;
pub mod karabiner_config;
pub mod keyfile;
pub mod keymap;
pub mod remap;
pub mod keys;
pub mod link;
pub mod macinject;
pub mod protocol;
pub mod router;
pub mod settings;
pub mod transport;
pub mod winsource;
