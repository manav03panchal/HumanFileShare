//! Networking module for HumanFileShare
//!
//! This module provides all networking functionality for peer-to-peer
//! file sharing, including:
//!
//! - **Endpoint**: Iroh endpoint management and device identity
//! - **Discovery**: Local network discovery via mDNS
//! - **Pairing**: Human-friendly pairing code generation and exchange
//! - **Transfer**: File transfer logic using iroh-blobs
//!
//! # Architecture
//!
//! The networking stack is built on [Iroh](https://iroh.computer/), which provides:
//! - NAT traversal and hole punching
//! - End-to-end encryption
//! - Efficient blob transfer
//!
//! # Example
//!
//! ```rust,ignore
//! use humanfileshare::net::{Endpoint, Discovery};
//!
//! // Initialize the endpoint (loads or creates device identity)
//! let endpoint = Endpoint::new().await?;
//!
//! // Start local network discovery
//! let discovery = Discovery::new(endpoint.device_id()).await?;
//! discovery.start_broadcasting().await?;
//! ```

pub mod discovery;
pub mod endpoint;
pub mod pairing;
pub mod transfer;

pub use discovery::DiscoveredPeer;
pub use discovery::Discovery;
pub use endpoint::Endpoint;
pub use pairing::{PairingCode, PairingSession};
pub use transfer::{
    TransferDirection, TransferHandle, TransferId, TransferManager, TransferProgress, TransferState,
};
