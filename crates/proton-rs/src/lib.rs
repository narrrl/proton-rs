//! Shared building blocks for the Proton Drive app family
//! (proton-drive-linux, Proton Gallery, Proton Stream).
//!
//! This crate only re-exports the `proton-*-rs` crates behind features. Apps
//! may depend on the individual crates directly instead.

#[cfg(feature = "auth")]
pub use proton_auth_rs as auth;
#[cfg(feature = "base")]
pub use proton_base_rs as base;
#[cfg(feature = "gallery-app")]
pub use proton_gallery_app_rs as gallery_app;
#[cfg(feature = "gallery-drive")]
pub use proton_gallery_drive_rs as gallery_drive;
#[cfg(feature = "gallery")]
pub use proton_gallery_rs as gallery;
