//! Iroh endpoint management and device identity
//!
//! This module handles the core networking endpoint using Iroh, including:
//! - Device identity (key pair) generation and persistence
//! - Endpoint initialization and configuration
//! - Connection management
//!
//! # Device Identity
//!
//! Each device has a unique cryptographic identity stored at:
//! `~/.config/humanfileshare/device_key`
//!
//! This identity persists across application restarts, ensuring consistent
//! device identification for pairing and transfers.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use iroh::{Endpoint as IrohEndpoint, NodeAddr, PublicKey, SecretKey};
use parking_lot::RwLock;
use thiserror::Error;
use tokio::fs;
use tracing::{debug, info, instrument, warn};

/// The ALPN protocol identifier for HumanFileShare
pub const ALPN: &[u8] = b"humanfileshare/1";

/// Default configuration directory name
const CONFIG_DIR: &str = "humanfileshare";

/// Device key filename
const DEVICE_KEY_FILE: &str = "device_key";

/// Errors that can occur during endpoint operations
#[derive(Error, Debug)]
pub enum EndpointError {
    /// Failed to create or access the configuration directory
    #[error("failed to access configuration directory: {0}")]
    ConfigDir(String),

    /// Failed to read or write the device key
    #[error("failed to persist device key: {0}")]
    KeyPersistence(String),

    /// Failed to initialize the Iroh endpoint
    #[error("failed to initialize endpoint: {0}")]
    EndpointInit(String),

    /// The endpoint is not initialized
    #[error("endpoint not initialized")]
    NotInitialized,

    /// The endpoint has been shut down
    #[error("endpoint has been shut down")]
    Shutdown,
}

/// Internal state of the endpoint
#[derive(Debug)]
enum EndpointState {
    /// Endpoint is not yet initialized
    Uninitialized,
    /// Endpoint is running
    Running(IrohEndpoint),
    /// Endpoint has been shut down
    Shutdown,
}

/// Manages the Iroh networking endpoint and device identity
///
/// The `Endpoint` struct is the primary interface for network operations.
/// It handles:
/// - Loading or generating a persistent device identity
/// - Initializing the Iroh endpoint with NAT traversal
/// - Providing the device's public key for identification
///
/// # Example
///
/// ```rust,ignore
/// use humanfileshare::net::Endpoint;
///
/// let endpoint = Endpoint::new().await?;
/// println!("Device ID: {}", endpoint.device_id());
/// ```
#[derive(Debug)]
pub struct Endpoint {
    /// The device's secret key (identity)
    secret_key: SecretKey,
    /// The device's public key (derived from secret key)
    public_key: PublicKey,
    /// The underlying Iroh endpoint
    state: Arc<RwLock<EndpointState>>,
}

impl Endpoint {
    /// Creates a new endpoint, loading or generating the device identity
    ///
    /// This will:
    /// 1. Load the existing device key from `~/.config/humanfileshare/device_key`
    /// 2. Or generate a new key if none exists
    /// 3. Initialize the Iroh endpoint
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The configuration directory cannot be created
    /// - The device key cannot be read or written
    /// - The Iroh endpoint fails to initialize
    #[instrument(name = "endpoint_new")]
    pub async fn new() -> Result<Self> {
        let secret_key = Self::load_or_create_device_key().await?;
        let public_key = secret_key.public();

        info!(device_id = %public_key, "Device identity loaded");

        let endpoint = IrohEndpoint::builder()
            .secret_key(secret_key.clone())
            .alpns(vec![ALPN.to_vec()])
            .bind()
            .await
            .context("failed to bind Iroh endpoint")?;

        debug!("Endpoint initialized");

        Ok(Self {
            secret_key,
            public_key,
            state: Arc::new(RwLock::new(EndpointState::Running(endpoint))),
        })
    }

    /// Returns the device's public key (unique device identifier)
    ///
    /// This key uniquely identifies this device across the network and
    /// remains consistent across application restarts.
    pub fn device_id(&self) -> PublicKey {
        self.public_key
    }

    /// Returns a reference to the secret key
    ///
    /// # Security
    ///
    /// This should only be used internally for cryptographic operations.
    pub(crate) fn secret_key(&self) -> &SecretKey {
        &self.secret_key
    }

    /// Returns the node address for this endpoint
    ///
    /// The node address contains the public key and network addresses
    /// needed for other peers to connect to this device.
    pub async fn node_addr(&self) -> Result<NodeAddr> {
        // Clone the endpoint inside the lock, then drop the lock before awaiting
        let endpoint = {
            let state = self.state.read();
            match &*state {
                EndpointState::Running(endpoint) => endpoint.clone(),
                EndpointState::Uninitialized => return Err(EndpointError::NotInitialized.into()),
                EndpointState::Shutdown => return Err(EndpointError::Shutdown.into()),
            }
        };
        Ok(endpoint.node_addr().await?)
    }

    /// Connects to a remote peer by their node address
    ///
    /// # Arguments
    ///
    /// * `addr` - The node address of the peer to connect to
    ///
    /// # Returns
    ///
    /// A connection to the remote peer
    #[instrument(skip(self), fields(peer = %addr.node_id))]
    pub async fn connect(&self, addr: NodeAddr) -> Result<iroh::endpoint::Connection> {
        let endpoint = {
            let state = self.state.read();
            match &*state {
                EndpointState::Running(endpoint) => endpoint.clone(),
                EndpointState::Uninitialized => return Err(EndpointError::NotInitialized.into()),
                EndpointState::Shutdown => return Err(EndpointError::Shutdown.into()),
            }
        };

        debug!("Connecting to peer");
        let connection = endpoint
            .connect(addr, ALPN)
            .await
            .context("failed to connect to peer")?;

        info!("Connected to peer successfully");
        Ok(connection)
    }

    /// Accepts an incoming connection
    ///
    /// # Returns
    ///
    /// The accepted connection, or an error if the endpoint is not running
    #[instrument(skip(self))]
    pub async fn accept(&self) -> Result<Option<iroh::endpoint::Connection>> {
        let endpoint = {
            let state = self.state.read();
            match &*state {
                EndpointState::Running(endpoint) => endpoint.clone(),
                EndpointState::Uninitialized => return Err(EndpointError::NotInitialized.into()),
                EndpointState::Shutdown => return Ok(None),
            }
        };

        if let Some(incoming) = endpoint.accept().await {
            let connection = incoming.await.context("failed to accept connection")?;
            if let Ok(peer_id) = connection.remote_node_id() {
                info!(peer = %peer_id, "Accepted incoming connection");
            } else {
                info!("Accepted incoming connection");
            }
            Ok(Some(connection))
        } else {
            Ok(None)
        }
    }

    /// Gracefully shuts down the endpoint
    ///
    /// This will:
    /// 1. Close all active connections
    /// 2. Stop accepting new connections
    /// 3. Release network resources
    #[instrument(skip(self))]
    pub async fn shutdown(&self) -> Result<()> {
        let endpoint = {
            let mut state = self.state.write();
            match std::mem::replace(&mut *state, EndpointState::Shutdown) {
                EndpointState::Running(endpoint) => endpoint,
                EndpointState::Uninitialized => {
                    warn!("Attempted to shut down uninitialized endpoint");
                    return Ok(());
                }
                EndpointState::Shutdown => {
                    warn!("Attempted to shut down already shutdown endpoint");
                    return Ok(());
                }
            }
        };

        info!("Shutting down endpoint");
        endpoint.close().await;
        info!("Endpoint shut down successfully");
        Ok(())
    }

    /// Checks if the endpoint is currently running
    pub fn is_running(&self) -> bool {
        matches!(&*self.state.read(), EndpointState::Running(_))
    }

    /// Returns a clone of the underlying Iroh endpoint for use with other protocols.
    ///
    /// # Errors
    ///
    /// Returns an error if the endpoint is not initialized or has been shut down.
    pub fn iroh_endpoint(&self) -> Result<IrohEndpoint> {
        let state = self.state.read();
        match &*state {
            EndpointState::Running(endpoint) => Ok(endpoint.clone()),
            EndpointState::Uninitialized => Err(EndpointError::NotInitialized.into()),
            EndpointState::Shutdown => Err(EndpointError::Shutdown.into()),
        }
    }

    /// Returns the local port the endpoint is bound to.
    ///
    /// This is useful for mDNS advertisement.
    pub fn bound_port(&self) -> Result<u16> {
        let state = self.state.read();
        match &*state {
            EndpointState::Running(endpoint) => {
                let (ipv4_addr, _ipv6_addr) = endpoint.bound_sockets();
                Ok(ipv4_addr.port())
            }
            EndpointState::Uninitialized => Err(EndpointError::NotInitialized.into()),
            EndpointState::Shutdown => Err(EndpointError::Shutdown.into()),
        }
    }

    /// Gets the path to the configuration directory
    fn config_dir() -> Result<PathBuf> {
        dirs::config_dir()
            .map(|p| p.join(CONFIG_DIR))
            .ok_or_else(|| {
                EndpointError::ConfigDir("could not determine config directory".into()).into()
            })
    }

    /// Gets the path to the device key file
    fn device_key_path() -> Result<PathBuf> {
        Ok(Self::config_dir()?.join(DEVICE_KEY_FILE))
    }

    /// Loads an existing device key or creates a new one
    #[instrument(name = "load_or_create_device_key")]
    async fn load_or_create_device_key() -> Result<SecretKey> {
        let key_path = Self::device_key_path()?;

        if key_path.exists() {
            debug!(path = %key_path.display(), "Loading existing device key");
            let key_bytes = fs::read(&key_path)
                .await
                .context("failed to read device key")?;

            let key_array: [u8; 32] = key_bytes
                .try_into()
                .map_err(|_| EndpointError::KeyPersistence("invalid key length".into()))?;

            let secret_key = SecretKey::from_bytes(&key_array);
            info!("Loaded existing device identity");
            Ok(secret_key)
        } else {
            debug!("No existing device key found, generating new identity");
            let secret_key = SecretKey::generate(rand::thread_rng());

            // Ensure config directory exists
            let config_dir = Self::config_dir()?;
            fs::create_dir_all(&config_dir)
                .await
                .context("failed to create config directory")?;

            // Write the key securely
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                let mut options = std::fs::OpenOptions::new();
                options.write(true).create(true).mode(0o600);
                let key_path_clone = key_path.clone();
                let key_bytes = secret_key.to_bytes();
                tokio::task::spawn_blocking(move || {
                    use std::io::Write;
                    let mut file = options.open(&key_path_clone)?;
                    file.write_all(&key_bytes)?;
                    Ok::<_, std::io::Error>(())
                })
                .await??;
            }

            #[cfg(not(unix))]
            {
                fs::write(&key_path, secret_key.to_bytes())
                    .await
                    .context("failed to write device key")?;
            }

            info!(path = %key_path.display(), "Generated and saved new device identity");
            Ok(secret_key)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_alpn_is_valid() {
        assert!(!ALPN.is_empty());
        assert!(ALPN.is_ascii());
    }
}
