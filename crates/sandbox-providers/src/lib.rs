//! Sandbox provider adapters for Orca Harness.
//!
//! The sandbox counterpart to `orca-harness-model-providers`: the kernel
//! owns the [`Sandbox`](orca_harness_core::Sandbox) and
//! [`Provisioner`](orca_harness_core::Provisioner) traits, and every
//! provider-specific detail lives here behind them.
//!
//! Providers converge on five operations — create, exec, stream, files,
//! destroy — which is what makes one trait viable. They diverge on two
//! things that stay inside their own modules: output framing (Connect
//! streams, ND-JSON, websockets) and file transfer encoding (raw bytes,
//! multipart, gzipped tarballs). Forcing a shared abstraction over those
//! would be indirection without reuse.
//!
//! They also diverge on something a host must act on rather than hide:
//! whether a live process accepts stdin. That is reported through
//! [`Capabilities`](orca_harness_core::Capabilities) so tool assembly can
//! refuse up front instead of failing on first use.

pub mod local;
pub mod spec;

pub use local::DockerProvisioner;
pub use spec::{EnvironmentSpec, Network, Packages, SetupCommand};
