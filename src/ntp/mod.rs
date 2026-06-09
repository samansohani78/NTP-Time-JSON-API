pub mod client;
pub mod protocol;
pub mod selection;
pub mod server;
pub mod stats;
pub mod sync;

// These re-exports are part of the crate's public API even if no
// internal consumer currently uses them in a way the compiler can see.
#[allow(unused_imports)]
pub use client::{NtpClient, NtpSample, PacketNtpClient};
#[allow(unused_imports)]
pub use protocol::{NtpPacket, ProtocolError, ntp_to_unix_ms, unix_ms_to_ntp};
pub use selection::SelectionDiagnostics;
pub use server::NtpServer;
pub use sync::{NtpSyncer, SyncOutcome, SyncQuality, SyncResult};
