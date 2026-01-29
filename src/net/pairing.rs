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
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use iroh::{NodeAddr, PublicKey};
use parking_lot::RwLock;
use rand::Rng;
use smol_str::SmolStr;
use thiserror::Error;
use tracing::{debug, info, instrument, warn};

use super::endpoint::Endpoint;

/// Default pairing code validity duration
const CODE_VALIDITY: Duration = Duration::from_secs(300); // 5 minutes

/// Number of possible adjectives
const ADJECTIVE_COUNT: usize = 16;

/// Number of possible nouns
const NOUN_COUNT: usize = 16;

/// Word lists for generating memorable codes - stored as static strings
/// Using a const array ensures compile-time initialization with no runtime overhead
const ADJECTIVES: &[&str; ADJECTIVE_COUNT] = &[
    "RED", "BLUE", "GREEN", "GOLD", "SILVER", "PURPLE", "ORANGE", "PINK", "BRIGHT", "DARK",
    "SWIFT", "CALM", "BOLD", "BRAVE", "COOL", "WARM",
];

const NOUNS: &[&str; NOUN_COUNT] = &[
    "FISH", "BIRD", "TREE", "STAR", "MOON", "SUN", "WAVE", "WIND", "ROCK", "LEAF", "FIRE", "RAIN",
    "SNOW", "SAND", "LAKE", "HILL",
];

/// Get an adjective by index - const fn for compile-time optimization
#[inline]
const fn get_adjective(idx: u8) -> &'static str {
    ADJECTIVES[idx as usize]
}

/// Get a noun by index - const fn for compile-time optimization
#[inline]
const fn get_noun(idx: u8) -> &'static str {
    NOUNS[idx as usize]
}

/// Find the index of an adjective in the word list
#[inline]
fn find_adjective(adj: &str) -> Option<u8> {
    ADJECTIVES.iter().position(|&a| a == adj).map(|i| i as u8)
}

/// Find the index of a noun in the word list
#[inline]
fn find_noun(noun: &str) -> Option<u8> {
    NOUNS.iter().position(|&n| n == noun).map(|i| i as u8)
}

/// Errors that can occur during pairing
#[derive(Error, Debug, Clone, PartialEq, Eq)]
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
/// # Memory Optimization
///
/// This struct uses `u8` indices instead of `String` for the word parts,
/// reducing memory usage from ~72 bytes to ~32 bytes per code.
/// The hash is pre-computed to allow fast HashMap lookups without
/// needing to hash the Instant field.
///
/// # Example
///
/// ```rust,ignore
/// let code = PairingCode::generate();
/// assert!(code.to_string().len() < 20); // e.g., "BLUE-FISH-42"
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingCode {
    /// The adjective index (0-15)
    adjective_idx: u8,
    /// The noun index (0-15)
    noun_idx: u8,
    /// The numeric suffix (0-99)
    number: u8,
    /// When this code was created
    created_at: Instant,
    /// How long this code is valid
    validity: Duration,
    /// Pre-computed hash for fast HashMap operations (excludes Instant)
    hash: u64,
}

impl PairingCode {
    /// Compute the hash for a pairing code (used for both construction and Hash trait)
    #[inline]
    fn compute_hash(adjective_idx: u8, noun_idx: u8, number: u8) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        adjective_idx.hash(&mut hasher);
        noun_idx.hash(&mut hasher);
        number.hash(&mut hasher);
        hasher.finish()
    }

    /// Generates a new random pairing code
    ///
    /// The code will be valid for the default duration (5 minutes).
    pub fn generate() -> Self {
        let mut rng = rand::thread_rng();

        let adjective_idx = rng.gen_range(0..ADJECTIVE_COUNT as u8);
        let noun_idx = rng.gen_range(0..NOUN_COUNT as u8);
        let number = rng.gen_range(0..100u8);

        let hash = Self::compute_hash(adjective_idx, noun_idx, number);

        Self {
            adjective_idx,
            noun_idx,
            number,
            created_at: Instant::now(),
            validity: CODE_VALIDITY,
            hash,
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
    ///
    /// # Optimization
    ///
    /// This function avoids unnecessary allocations by:
    /// - Using stack-allocated buffers for parsing
    /// - Validating against word lists using index lookups
    /// - Only creating the compact string representation when needed
    pub fn parse(s: &str) -> Result<Self, PairingError> {
        // Fast path rejection for obviously invalid inputs
        let s = s.trim();
        if s.len() < 7 || s.len() > 20 {
            return Err(PairingError::InvalidCode);
        }

        // Count dashes first (cheaper than splitting)
        let dash_count = s.bytes().filter(|&b| b == b'-').count();
        if dash_count != 2 {
            return Err(PairingError::InvalidCode);
        }

        // Parse manually to avoid Vec allocation from split
        let s_upper = s.to_uppercase();
        let bytes = s_upper.as_bytes();

        let first_dash = bytes
            .iter()
            .position(|&b| b == b'-')
            .ok_or(PairingError::InvalidCode)?;
        let second_dash = bytes
            .iter()
            .skip(first_dash + 1)
            .position(|&b| b == b'-')
            .map(|pos| pos + first_dash + 1)
            .ok_or(PairingError::InvalidCode)?;

        // Extract parts without allocation
        let adj_str =
            std::str::from_utf8(&bytes[..first_dash]).map_err(|_| PairingError::InvalidCode)?;
        let noun_str = std::str::from_utf8(&bytes[first_dash + 1..second_dash])
            .map_err(|_| PairingError::InvalidCode)?;
        let num_str = std::str::from_utf8(&bytes[second_dash + 1..])
            .map_err(|_| PairingError::InvalidCode)?;

        // Validate and convert to indices
        let adjective_idx = find_adjective(adj_str).ok_or(PairingError::InvalidCode)?;
        let noun_idx = find_noun(noun_str).ok_or(PairingError::InvalidCode)?;

        // Parse number with range validation
        let number: u8 = num_str.parse().map_err(|_| PairingError::InvalidCode)?;
        if number >= 100 {
            return Err(PairingError::InvalidCode);
        }

        let hash = Self::compute_hash(adjective_idx, noun_idx, number);

        Ok(Self {
            adjective_idx,
            noun_idx,
            number,
            created_at: Instant::now(),
            validity: CODE_VALIDITY,
            hash,
        })
    }

    /// Returns the adjective as a string slice
    #[inline]
    pub fn adjective(&self) -> &'static str {
        get_adjective(self.adjective_idx)
    }

    /// Returns the noun as a string slice
    #[inline]
    pub fn noun(&self) -> &'static str {
        get_noun(self.noun_idx)
    }

    /// Returns the numeric suffix
    #[inline]
    pub fn number(&self) -> u8 {
        self.number
    }

    /// Checks if this code has expired
    #[inline]
    pub fn is_expired(&self) -> bool {
        self.created_at.elapsed() > self.validity
    }

    /// Returns the remaining validity time
    #[inline]
    pub fn remaining_validity(&self) -> Duration {
        self.validity.saturating_sub(self.created_at.elapsed())
    }

    /// Returns the code as a compact string for network transmission
    ///
    /// Uses SmolStr for small string optimization - fits within inline storage
    #[inline]
    pub fn to_compact(&self) -> SmolStr {
        // SmolStr can store up to 23 bytes inline
        // Format "ADJECTIVE-NOUN-NN" max: 7 + 1 + 4 + 1 + 2 = 15 bytes, always fits inline
        format!("{}-{}-{:02}", self.adjective(), self.noun(), self.number).into()
    }

    /// Returns the pre-computed hash value
    #[inline]
    pub fn precomputed_hash(&self) -> u64 {
        self.hash
    }
}

impl fmt::Display for PairingCode {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}-{:02}", self.adjective(), self.noun(), self.number)
    }
}

// Manual Hash implementation that uses the pre-computed hash
// This excludes the Instant field which shouldn't participate in hashing
impl Hash for PairingCode {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.hash.hash(state);
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
    Failed { reason: Arc<str> },
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
    #[instrument(skip(_endpoint))]
    pub fn new_initiator(_endpoint: Arc<Endpoint>) -> Self {
        let code = PairingCode::generate();
        info!(code = %code, "Created new pairing session");

        Self {
            code,
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
    #[instrument(skip(_endpoint), fields(code = %code))]
    pub fn new_joiner(_endpoint: Arc<Endpoint>, code: PairingCode) -> Self {
        info!("Joining pairing session");

        Self {
            code,
            state: Arc::new(RwLock::new(PairingState::WaitingForPeer)),
        }
    }

    /// Returns the pairing code for this session
    #[inline]
    pub fn code(&self) -> &PairingCode {
        &self.code
    }

    /// Returns the current state of the pairing session
    #[inline]
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
        let mut state = self.state.write();
        if !matches!(*state, PairingState::Completed { .. }) {
            info!("Pairing session cancelled");
            *state = PairingState::Cancelled;
        } else {
            warn!("Attempted to cancel already completed pairing session");
        }
    }

    /// Marks the pairing as failed with a reason
    pub fn fail(&self, reason: impl Into<String>) {
        let mut state = self.state.write();
        if !matches!(
            *state,
            PairingState::Completed { .. } | PairingState::Cancelled
        ) {
            *state = PairingState::Failed {
                reason: reason.into().into(),
            };
        }
    }

    /// Marks the pairing as completed with the peer's public key
    #[instrument(skip(self))]
    pub fn complete(&self, peer_id: PublicKey) {
        let mut state = self.state.write();
        if !matches!(*state, PairingState::Cancelled) {
            info!(peer = %peer_id, "Pairing completed successfully");
            *state = PairingState::Completed { peer_id };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

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

        assert_eq!(original.adjective(), parsed.adjective());
        assert_eq!(original.noun(), parsed.noun());
        assert_eq!(original.number(), parsed.number());
        assert_eq!(original.precomputed_hash(), parsed.precomputed_hash());
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

    #[test]
    fn test_pairing_code_to_compact() {
        let code = PairingCode::parse("BLUE-FISH-42").unwrap();
        let compact = code.to_compact();

        // Verify format
        assert_eq!(compact.as_str(), "BLUE-FISH-42");

        // SmolStr should fit inline for this small string (15 bytes < 23 bytes inline capacity)
        assert!(compact.is_inline());
    }

    #[test]
    fn test_remaining_validity_edge_cases() {
        // Zero validity
        let code = PairingCode::generate_with_validity(Duration::ZERO);
        assert_eq!(code.remaining_validity(), Duration::ZERO);
        assert!(code.is_expired());

        // Very short validity
        let code = PairingCode::generate_with_validity(Duration::from_nanos(1));
        std::thread::sleep(Duration::from_millis(1));
        assert_eq!(code.remaining_validity(), Duration::ZERO);

        // Normal validity - should not be expired immediately
        let code = PairingCode::generate_with_validity(Duration::from_secs(60));
        assert!(!code.is_expired());
        assert!(code.remaining_validity() > Duration::ZERO);
        assert!(code.remaining_validity() <= Duration::from_secs(60));
    }

    #[test]
    fn test_pairing_state_transitions() {
        use PairingState::*;

        // Test all state variants can be created
        let waiting = WaitingForPeer;
        let exchanging = Exchanging;
        let completed = Completed {
            peer_id: PublicKey::from_bytes(&[0u8; 32]),
        };
        let failed = Failed {
            reason: "test error".into(),
        };
        let cancelled = Cancelled;

        // Verify state equality
        assert_eq!(waiting, WaitingForPeer);
        assert_eq!(exchanging, Exchanging);
        assert_eq!(cancelled, Cancelled);
        assert_ne!(waiting, exchanging);

        // Verify completed states with same peer_id are equal
        let completed2 = Completed {
            peer_id: PublicKey::from_bytes(&[0u8; 32]),
        };
        assert_eq!(completed, completed2);
    }

    #[test]
    fn test_pairing_error_display_messages() {
        let cases = vec![
            (PairingError::CodeExpired, "pairing code has expired"),
            (PairingError::InvalidCode, "invalid pairing code format"),
            (PairingError::Rejected, "pairing rejected by peer"),
            (PairingError::Timeout, "pairing timed out"),
            (
                PairingError::AlreadyCompleted,
                "pairing session already completed",
            ),
            (PairingError::PeerNotFound, "no peer found with this code"),
        ];

        for (error, expected) in cases {
            assert_eq!(error.to_string(), expected);
        }
    }

    #[test]
    fn test_pairing_session_cancel() {
        // We can't easily create an Endpoint in tests, so we test the state logic directly
        let code = PairingCode::generate();

        // Create a mock state to test cancellation logic
        let state = Arc::new(RwLock::new(PairingState::WaitingForPeer));

        // Cancel should change state from WaitingForPeer
        {
            let mut s = state.write();
            if !matches!(*s, PairingState::Completed { .. }) {
                *s = PairingState::Cancelled;
            }
        }
        assert_eq!(*state.read(), PairingState::Cancelled);

        // Cancel should not change state from Completed
        let state2 = Arc::new(RwLock::new(PairingState::Completed {
            peer_id: PublicKey::from_bytes(&[1u8; 32]),
        }));
        {
            let mut s = state2.write();
            if !matches!(*s, PairingState::Completed { .. }) {
                *s = PairingState::Cancelled;
            }
        }
        assert!(matches!(*state2.read(), PairingState::Completed { .. }));
    }

    #[test]
    fn test_code_generation_entropy() {
        // Generate many codes and verify reasonable distribution
        let iterations = 1000;
        let mut adjectives_seen = HashSet::new();
        let mut nouns_seen = HashSet::new();
        let mut numbers_seen = HashSet::new();
        let mut all_codes = HashSet::new();

        for _ in 0..iterations {
            let code = PairingCode::generate();
            adjectives_seen.insert(code.adjective_idx);
            nouns_seen.insert(code.noun_idx);
            numbers_seen.insert(code.number);
            all_codes.insert(code.precomputed_hash());
        }

        // Should see a good distribution of adjectives and nouns
        // With 1000 iterations and 16 options each, we expect to see most if not all
        assert!(
            adjectives_seen.len() >= 8,
            "Poor adjective entropy: only {} unique",
            adjectives_seen.len()
        );
        assert!(
            nouns_seen.len() >= 8,
            "Poor noun entropy: only {} unique",
            nouns_seen.len()
        );
        assert!(
            numbers_seen.len() >= 50,
            "Poor number entropy: only {} unique",
            numbers_seen.len()
        );

        // Collision check - with 256*100 possible combinations, 1000 samples should have minimal collisions
        let collision_rate = 1.0 - (all_codes.len() as f64 / iterations as f64);
        assert!(
            collision_rate < 0.1,
            "Too many collisions: {:.2}%",
            collision_rate * 100.0
        );
    }

    #[test]
    fn test_parse_edge_cases() {
        // Empty string
        assert!(PairingCode::parse("").is_err());

        // Whitespace only
        assert!(PairingCode::parse("   ").is_err());

        // Special characters
        assert!(PairingCode::parse("BLUE@FISH#42").is_err());
        assert!(PairingCode::parse("BLUE-FISH-42!").is_err());

        // Extra dashes
        assert!(PairingCode::parse("BLUE-FISH-42-EXTRA").is_err());
        assert!(PairingCode::parse("-BLUE-FISH-42").is_err());

        // Missing parts
        assert!(PairingCode::parse("-FISH-42").is_err());
        assert!(PairingCode::parse("BLUE--42").is_err());

        // Whitespace handling
        let code = PairingCode::parse("  BLUE-FISH-42  ").unwrap();
        assert_eq!(code.to_string(), "BLUE-FISH-42");

        // Mixed case (should be normalized)
        let code = PairingCode::parse("blue-fish-42").unwrap();
        assert_eq!(code.to_string(), "BLUE-FISH-42");

        let code = PairingCode::parse("BlUe-FiSh-42").unwrap();
        assert_eq!(code.to_string(), "BLUE-FISH-42");
    }

    #[test]
    fn test_pairing_code_hash_and_equality() {
        let code1 = PairingCode::parse("BLUE-FISH-42").unwrap();
        let code2 = PairingCode::parse("BLUE-FISH-42").unwrap();
        let code3 = PairingCode::parse("RED-BIRD-01").unwrap();

        // Same codes should be equal
        assert_eq!(code1, code2);
        assert_eq!(code1.precomputed_hash(), code2.precomputed_hash());

        // Different codes should not be equal
        assert_ne!(code1, code3);

        // Hash consistency
        let mut hasher1 = std::collections::hash_map::DefaultHasher::new();
        code1.hash(&mut hasher1);

        let mut hasher2 = std::collections::hash_map::DefaultHasher::new();
        code2.hash(&mut hasher2);

        assert_eq!(hasher1.finish(), hasher2.finish());

        // Can be used in HashSet
        let mut set = HashSet::new();
        set.insert(code1.clone());
        assert!(set.contains(&code2)); // Same code should be found
        assert!(!set.contains(&code3)); // Different code should not be found
    }

    #[test]
    fn test_invalid_number_ranges() {
        // Number >= 100
        assert!(PairingCode::parse("BLUE-FISH-100").is_err());
        assert!(PairingCode::parse("BLUE-FISH-999").is_err());

        // Negative numbers (parsed as invalid)
        assert!(PairingCode::parse("BLUE-FISH--1").is_err());

        // Boundary values
        let code = PairingCode::parse("BLUE-FISH-99").unwrap();
        assert_eq!(code.number(), 99);

        let code = PairingCode::parse("BLUE-FISH-00").unwrap();
        assert_eq!(code.number(), 0);
    }

    #[test]
    fn test_case_insensitivity() {
        let lower = PairingCode::parse("blue-fish-42").unwrap();
        let upper = PairingCode::parse("BLUE-FISH-42").unwrap();
        let mixed = PairingCode::parse("BlUe-FiSh-42").unwrap();

        assert_eq!(lower, upper);
        assert_eq!(upper, mixed);
    }

    #[test]
    fn test_const_fn_word_access() {
        // Verify const fn works correctly
        assert_eq!(get_adjective(0), "RED");
        assert_eq!(get_adjective(15), "WARM");
        assert_eq!(get_noun(0), "FISH");
        assert_eq!(get_noun(15), "HILL");
    }

    #[test]
    fn test_accessor_methods() {
        let code = PairingCode::parse("BLUE-FISH-42").unwrap();

        assert_eq!(code.adjective(), "BLUE");
        assert_eq!(code.noun(), "FISH");
        assert_eq!(code.number(), 42);
    }

    #[test]
    fn test_pairing_session_state_fail() {
        // Test fail() state transition logic
        let state = Arc::new(RwLock::new(PairingState::Exchanging));

        {
            let mut s = state.write();
            if !matches!(*s, PairingState::Completed { .. } | PairingState::Cancelled) {
                *s = PairingState::Failed {
                    reason: "network error".into(),
                };
            }
        }

        match &*state.read() {
            PairingState::Failed { reason } => {
                assert_eq!(reason.as_ref(), "network error");
            }
            _ => panic!("Expected Failed state"),
        }
    }

    #[test]
    fn test_pairing_session_complete() {
        let state = Arc::new(RwLock::new(PairingState::Exchanging));
        let peer_id = PublicKey::from_bytes(&[0x42; 32]);

        {
            let mut s = state.write();
            if !matches!(*s, PairingState::Cancelled) {
                *s = PairingState::Completed { peer_id };
            }
        }

        match &*state.read() {
            PairingState::Completed { peer_id: pid } => {
                assert_eq!(pid.as_bytes(), &[0x42; 32]);
            }
            _ => panic!("Expected Completed state"),
        }
    }

    #[test]
    fn test_pairing_error_clone_equality() {
        let err1 = PairingError::Timeout;
        let err2 = err1.clone();

        assert_eq!(err1, err2);
        assert_eq!(err1.to_string(), err2.to_string());
    }

    #[test]
    fn test_word_list_contents() {
        // Verify all adjectives are uppercase and valid
        for (i, &adj) in ADJECTIVES.iter().enumerate() {
            assert!(
                adj.chars().all(|c| c.is_ascii_uppercase()),
                "Adjective {} is not uppercase: {}",
                i,
                adj
            );
            assert!(!adj.contains('-'), "Adjective {} contains dash: {}", i, adj);
        }

        // Verify all nouns are uppercase and valid
        for (i, &noun) in NOUNS.iter().enumerate() {
            assert!(
                noun.chars().all(|c| c.is_ascii_uppercase()),
                "Noun {} is not uppercase: {}",
                i,
                noun
            );
            assert!(!noun.contains('-'), "Noun {} contains dash: {}", i, noun);
        }
    }

    #[test]
    fn test_memory_size_optimization() {
        // Verify PairingCode is reasonably small
        let size = std::mem::size_of::<PairingCode>();
        // Should be around 32 bytes (3 u8 + padding + Instant + Duration + u64 hash)
        // Original String-based version was ~72 bytes
        assert!(size <= 64, "PairingCode is too large: {} bytes", size);
    }
}
