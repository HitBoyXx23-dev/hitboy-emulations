//! Nintendo Switch: game package containers (NSP = PFS0, XCI = HFS0 partitions) and the
//! user's key file. The ARM64 CPU and Horizon OS services come next.

pub mod a64;
pub mod aes;
pub mod horizon;
pub mod ipc;
pub mod keys;
pub mod launch;
pub mod nca;
pub mod nso;
pub mod pfs;
pub mod romfs;
