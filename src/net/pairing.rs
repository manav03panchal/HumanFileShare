//! Pairing code generation and exchange
//!
//! This module handles the human-friendly pairing process between devices.
//! Instead of exchanging complex public keys, users share short, memorable
//! codes like "BLUE-FISH-42".
//!
//! # How Pairing Works
//!
//! 1. Device A generates a pairing code and displays it to the user
//! 2. User verbally shares the code with Device B's user
//! 3. Device B enters the code
//! 4. Both devices exchange their full public keys securely
//! 5. Devices are now paired and can transfer files
//!
//! # Security
//!
//! The pairing code provides a limited-time, limited-use authentication
//! mechanism. Once pairing is complete, subsequent connections use the
//! full public key for authentication.
//!
//! # Example
//!
//! ```rust,ignore
//! use humanfileshare::net::pairing::{PairingCode, PairingSession};
//!
//! // Device A: Generate and display code
//! let code = PairingCode::generate();
//! println!("Share this code: {}", code);
//!
//! // Create a pairing session
//! let session = PairingSession::new(endpoint, code).await?;
//! let peer = session.wait_for_peer().await?;
//! ```

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use iroh::{NodeAddr, PublicKey};
use parking_lot::RwLock;
use rand::Rng;
use thiserror::Error;
use tracing::{debug, info, instrument, warn};

use super::endpoint::Endpoint;

/// Default pairing code validity duration
const CODE_VALIDITY: Duration = Duration::from_secs(300); // 5 minutes

/// Word lists for generating memorable codes
const ADJECTIVES: &[&str] = &[
    "RED", "BLUE", "GREEN", "GOLD", "SILVER", "PURPLE", "ORANGE", "PINK", "BRIGHT", "DARK",
    "SWIFT", "CALM", "BOLD", "BRAVE", "COOL", "WARM",
];

const NOUNS: &[&str] = &[
    "FISH", "BIRD", "TREE", "STAR", "MOON", "SUN", "WAVE", "WIND", "ROCK", "LEAF", "FIRE", "RAIN",
    "SNOW", "SAND", "LAKE", "HILL",
];

/// Errors that can occur during pairing
#[derive(Error, Debug)]
pub enum PairingError {
    /// The pairing code has expired
    #[error("pairing code has expired")]
    CodeExpired,

    /// The pairing code is invalid
    #[error("invalid pairing code format")]
    InvalidCode,

    /// Pairing was rejected by the peer
    #[error("pairing rejected by peer")]
    Rejected,

    /// Pairing timed out waiting for peer
    #[error("pairing timed out")]
    Timeout,

    /// Pairing session already completed
    #[error("pairing session already completed")]
    AlreadyCompleted,

    /// No matching peer found for this code
    #[error("no peer found with this code")]
    PeerNotFound,
}

/// A human-friendly pairing code
///
/// Pairing codes are short, memorable strings that can be easily
/// spoken aloud or typed. Format: "ADJECTIVE-NOUN-NUMBER"
///
/// # Example
///
/// ```rust,ignore
/// let code = PairingCode::generate();
/// assert!(code.to_string().len() < 20); // e.g., "BLUE-FISH-42"
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PairingCode {
    /// The adjective part
    adjective: String,
    /// The noun part
    noun: String,
    /// The numeric suffix (0-99)
    number: u8,
    /// When this code was created
    created_at: Instant,
    /// How long this code is valid
    validity: Duration,
}

impl PairingCode {
    /// Generates a new random pairing code
    ///
    /// The code will be valid for the default duration (5 minutes).
    pub fn generate() -> Self {
        let mut rng = rand::thread_rng();

        let adjective = ADJECTIVES[rng.gen_range(0..ADJECTIVES.len())].to_string();
        let noun = NOUNS[rng.gen_range(0..NOUNS.len())].to_string();
        let number = rng.gen_range(0..100);

        Self {
            adjective,
            noun,
            number,
            created_at: Instant::now(),
            validity: CODE_VALIDITY,
        }
    }

    /// Creates a pairing code with custom validity duration
    pub fn generate_with_validity(validity: Duration) -> Self {
        let mut code = Self::generate();
        code.validity = validity;
        code
    }

    /// Parses a pairing code from a string
    ///
    /// # Arguments
    ///
    /// * `s` - The string to parse (e.g., "BLUE-FISH-42")
    ///
    /// # Errors
    ///
    /// Returns an error if the string is not a valid pairing code format
    pub fn parse(s: &str) -> Result<Self, PairingError> {
        let s = s.trim().to_uppercase();
        let parts: Vec<&str> = s.split('-').collect();

        if parts.len() != 3 {
            return Err(PairingError::InvalidCode);
        }

        let adjective = parts[0].to_string();
        let noun = parts[1].to_string();
        let number: u8 = parts[2].parse().map_err(|_| PairingError::InvalidCode)?;

        // Validate that adjective and noun are in our word lists
        if !ADJECTIVES.contains(&adjective.as_str()) {
            return Err(PairingError::InvalidCode);
        }
        if !NOUNS.contains(&noun.as_str()) {
            return Err(PairingError::InvalidCode);
        }

        Ok(Self {
            adjective,
            noun,
            number,
            created_at: Instant::now(),
            validity: CODE_VALIDITY,
        })
    }

    /// Checks if this code has expired
    pub fn is_expired(&self) -> bool {
        self.created_at.elapsed() > self.validity
    }

    /// Returns the remaining validity time
    pub fn remaining_validity(&self) -> Duration {
        self.validity.saturating_sub(self.created_at.elapsed())
    }

    /// Returns the code as a compact string for network transmission
    pub fn to_compact(&self) -> String {
        format!("{}-{}-{:02}", self.adjective, self.noun, self.number)
    }
}

impl fmt::Display for PairingCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}-{:02}", self.adjective, self.noun, self.number)
    }
}

/// State of a pairing session
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingState {
    /// Waiting for peer to connect
    WaitingForPeer,
    /// Peer connected, exchanging keys
    Exchanging,
    /// Pairing completed successfully
    Completed { peer_id: PublicKey },
    /// Pairing failed
    Failed { reason: String },
    /// Pairing was cancelled
    Cancelled,
}

/// A pairing session manages the process of connecting two devices
///
/// The session handles:
/// - Advertising the pairing code
/// - Accepting incoming pairing requests
/// - Exchanging and verifying public keys
/// - Establishing the initial connection
#[derive(Debug)]
pub struct PairingSession {
    /// The pairing code for this session
    code: PairingCode,
    /// Reference to the endpoint
    endpoint: Arc<Endpoint>,
    /// Current state of the pairing
    state: Arc<RwLock<PairingState>>,
}

impl PairingSession {
    /// Creates a new pairing session as the initiator (code generator)
    ///
    /// # Arguments
    ///
    /// * `endpoint` - The network endpoint to use
    ///
    /// # Returns
    ///
    /// A new pairing session with a generated code
    #[instrument(skip(endpoint))]
    pub fn new_initiator(endpoint: Arc<Endpoint>) -> Self {
        let code = PairingCode::generate();
        info!(code = %code, "Created new pairing session");

        Self {
            code,
            endpoint,
            state: Arc::new(RwLock::new(PairingState::WaitingForPeer)),
        }
    }

    /// Creates a new pairing session as the joiner (code enterer)
    ///
    /// # Arguments
    ///
    /// * `endpoint` - The network endpoint to use
    /// * `code` - The pairing code received from the initiator
    ///
    /// # Errors
    ///
    /// Returns an error if the code is invalid
    #[instrument(skip(endpoint), fields(code = %code))]
    pub fn new_joiner(endpoint: Arc<Endpoint>, code: PairingCode) -> Self {
        info!("Joining pairing session");

        Self {
            code,
            endpoint,
            state: Arc::new(RwLock::new(PairingState::WaitingForPeer)),
        }
    }

    /// Returns the pairing code for this session
    pub fn code(&self) -> &PairingCode {
        &self.code
    }

    /// Returns the current state of the pairing session
    pub fn state(&self) -> PairingState {
        self.state.read().clone()
    }

    /// Waits for a peer to connect and complete pairing
    ///
    /// This is used by the initiator (code generator) to wait for
    /// someone to enter their code.
    ///
    /// # Returns
    ///
    /// The public key of the paired peer
    ///
    /// # Errors
    ///
    /// Returns an error if pairing fails or times out
    #[instrument(skip(self))]
    pub async fn wait_for_peer(&self) -> Result<PublicKey> {
        if self.code.is_expired() {
            return Err(PairingError::CodeExpired.into());
        }

        let current_state = self.state.read().clone();
        if let PairingState::Completed { peer_id } = current_state {
            return Ok(peer_id);
        }
        if matches!(
            current_state,
            PairingState::Failed { .. } | PairingState::Cancelled
        ) {
            bail!("Pairing session is no longer active");
        }

        debug!("Waiting for peer to connect");

        // TODO: Implement actual pairing protocol
        // This would involve:
        // 1. Advertising our pairing code via mDNS or relay
        // 2. Accepting incoming connections
        // 3. Verifying the peer knows our code
        // 4. Exchanging public keys
        // 5. Establishing trust

        // For now, return a timeout error as placeholder
        tokio::time::sleep(self.code.remaining_validity()).await;
        Err(PairingError::Timeout.into())
    }

    /// Connects to a peer using their pairing code
    ///
    /// This is used by the joiner (code enterer) to connect to
    /// the device that generated the code.
    ///
    /// # Arguments
    ///
    /// * `peer_addr` - The network address of the peer (if known)
    ///
    /// # Returns
    ///
    /// The public key of the paired peer
    ///
    /// # Errors
    ///
    /// Returns an error if the code doesn't match or connection fails
    #[instrument(skip(self))]
    pub async fn connect_to_peer(&self, peer_addr: Option<NodeAddr>) -> Result<PublicKey> {
        if self.code.is_expired() {
            return Err(PairingError::CodeExpired.into());
        }

        debug!("Attempting to connect to peer");
        *self.state.write() = PairingState::Exchanging;

        // TODO: Implement actual pairing connection
        // This would involve:
        // 1. Finding the peer via mDNS or relay using the code
        // 2. Connecting to the peer
        // 3. Proving we know the pairing code
        // 4. Receiving their public key
        // 5. Establishing trust

        // For now, return peer not found as placeholder
        Err(PairingError::PeerNotFound.into())
    }

    /// Cancels the pairing session
    #[instrument(skip(self))]
    pub fn cancel(&self) {
        info!("Pairing session cancelled");
        *self.state.write() = PairingState::Cancelled;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pairing_code_generation() {
        let code = PairingCode::generate();
        let code_str = code.to_string();

        // Should have format ADJECTIVE-NOUN-NN
        let parts: Vec<&str> = code_str.split('-').collect();
        assert_eq!(parts.len(), 3);
        assert!(ADJECTIVES.contains(&parts[0]));
        assert!(NOUNS.contains(&parts[1]));

        let number: u8 = parts[2].parse().unwrap();
        assert!(number < 100);
    }

    #[test]
    fn test_pairing_code_parse() {
        let original = PairingCode::generate();
        let code_str = original.to_string();
        let parsed = PairingCode::parse(&code_str).unwrap();

        assert_eq!(original.adjective, parsed.adjective);
        assert_eq!(original.noun, parsed.noun);
        assert_eq!(original.number, parsed.number);
    }

    #[test]
    fn test_pairing_code_parse_invalid() {
        assert!(PairingCode::parse("INVALID").is_err());
        assert!(PairingCode::parse("BLUE-FISH").is_err());
        assert!(PairingCode::parse("NOTAWORD-FISH-42").is_err());
        assert!(PairingCode::parse("BLUE-NOTANOUN-42").is_err());
        assert!(PairingCode::parse("BLUE-FISH-ABC").is_err());
    }

    #[test]
    fn test_pairing_code_expiry() {
        let code = PairingCode::generate_with_validity(Duration::from_millis(1));
        std::thread::sleep(Duration::from_millis(10));
        assert!(code.is_expired());
    }
}
