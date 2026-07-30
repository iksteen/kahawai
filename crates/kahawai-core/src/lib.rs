pub mod enroll;
pub mod media;
pub mod names;
pub mod pki;

/// This binary's build stamp — "<short-hash>[+dirty] <commit-date>",
/// stamped at compile time (see build.rs). Carried in the AR-7 Hello and
/// logged by the hub, so "which build is that box running?" is answered
/// from the hub's log.
pub fn build_stamp() -> &'static str {
    env!("KAHAWAI_BUILD")
}
