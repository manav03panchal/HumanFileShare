//! HumanFileShare - Cross-platform file sharing made simple
#![recursion_limit = "2048"]
//!
//! A minimalist, elegant file sharing application that abstracts away
//! all complexity from the user.
//!
//! # Architecture
//!
//! The application is organized into several modules:
//!
//! - [`app`]: Core application state and lifecycle management
//! - [`ui`]: User interface components built with GPUI
//! - [`net`]: Networking layer using Iroh for P2P communication
//!
//! # Networking
//!
//! The networking stack is built on [Iroh](https://iroh.computer/) and provides:
//!
//! - **Device Identity**: Persistent cryptographic identity stored locally
//! - **Discovery**: mDNS-based local network discovery
//! - **Pairing**: Human-friendly pairing codes (e.g., "BLUE-FISH-42")
//! - **Transfer**: Efficient blob-based file transfers
//!
//! # Example
//!
//! ```rust,ignore
//! use humanfileshare::net::{Endpoint, Discovery, TransferManager};
//!
//! // Initialize networking
//! let endpoint = Endpoint::new().await?;
//! let discovery = Discovery::new(endpoint.device_id());
//! discovery.start()?;
//!
//! // Transfer files
//! let transfer = TransferManager::new(endpoint.into()).await?;
//! let handle = transfer.send_file(peer_id, "photo.jpg").await?;
//! ```

pub mod app;
pub mod net;
pub mod ui;

pub use app::App;
