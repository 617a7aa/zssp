use crate::crypto::{secure_eq, HashSha512};
use crate::proto::*;
use std::ops::Deref;

#[derive(Clone, PartialEq, Eq)]
pub enum RatchetState {
    Null,
    Empty,
    NonEmpty(NonEmptyRatchetState),
}
use zeroize::Zeroizing;
use RatchetState::*;
impl RatchetState {
    /// Helper function to create a new nonempty ratchet state from a raw ratchet key, fingerprint,
    /// and the ratchet chain's current length.
    pub fn new_nonempty(key: Zeroizing<[u8; RATCHET_SIZE]>, fingerprint: Zeroizing<[u8; RATCHET_SIZE]>, chain_len: u64) -> Self {
        NonEmpty(NonEmptyRatchetState { key, fingerprint, chain_len })
    }
    /// Creates the set of two ratchet states that ZSSP initializes a key exchange with when
    /// communicating to a brand new peer.
    pub fn new_initial_states() -> [RatchetState; 2] {
        [RatchetState::Empty, RatchetState::Null]
    }
    /// Creates a set of two ratchet states that ZSSP can initialize a key exchange with when
    /// communicating to a brand new peer.
    /// The peer must know the one-time-password and initialize their key exchange with it as well.
    pub fn new_from_otp<Hmac: HashSha512>(otp: &[u8]) -> [RatchetState; 2] {
        let mut buffer = Vec::new();
        buffer.push(1);
        buffer.extend(LABEL_OTP_TO_RATCHET);
        buffer.push(0);
        buffer.extend((2u16 * 512u16).to_be_bytes());
        let r1 = Hmac::hmac(otp, &buffer);
        buffer[0] = 2;
        let r2 = Hmac::hmac(otp, &buffer);
        [
            Self::new_nonempty(
                Zeroizing::new(r1[..RATCHET_SIZE].try_into().unwrap()),
                Zeroizing::new(r2[..RATCHET_SIZE].try_into().unwrap()),
                1,
            ),
            RatchetState::Null,
        ]
    }
    /// Returns true if this ratchet state is the null ratchet state.
    pub fn is_null(&self) -> bool {
        matches!(self, Null)
    }
    /// Returns true if this ratchet state is the empty ratchet state. The empty ratchet state has
    /// a key of all zeros and the empty string as the ratchet fingerprint.
    pub fn is_empty(&self) -> bool {
        matches!(self, Empty)
    }
    /// Retrieve a nonempty ratchet state if it exists.
    pub fn nonempty(&self) -> Option<&NonEmptyRatchetState> {
        match self {
            NonEmpty(rs) => Some(rs),
            _ => None,
        }
    }
    /// Retrieve the ratchet chain length, or 0 if this ratchet state is null or empty.
    pub fn chain_len(&self) -> u64 {
        self.nonempty().map_or(0, |rs| rs.chain_len)
    }
    /// Retrieve the ratchet fingerprint if it exists.
    pub fn fingerprint(&self) -> Option<&[u8; RATCHET_SIZE]> {
        self.nonempty().map(|rs| rs.fingerprint.deref())
    }
    /// Retrieve the ratchet key if it exists.
    /// This function will return a key of all zeros if this ratchet state is the empty ratchet state.
    pub fn key(&self) -> Option<&[u8; RATCHET_SIZE]> {
        const ZERO_KEY: [u8; RATCHET_SIZE] = [0u8; RATCHET_SIZE];
        match self {
            Null => None,
            Empty => Some(&ZERO_KEY),
            NonEmpty(rs) => Some(&rs.key),
        }
    }
}
/// A ratchet key and fingerprint,
/// along with the length of the ratchet chain the keys were derived from.
#[derive(Clone, Eq)]
pub struct NonEmptyRatchetState {
    pub key: Zeroizing<[u8; RATCHET_SIZE]>,
    pub fingerprint: Zeroizing<[u8; RATCHET_SIZE]>,
    pub chain_len: u64,
}
impl PartialEq for NonEmptyRatchetState {
    fn eq(&self, other: &Self) -> bool {
        secure_eq(&self.key, &other.key) && secure_eq(&self.fingerprint, &other.fingerprint) && self.chain_len == other.chain_len
    }
}
