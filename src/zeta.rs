use std::num::NonZeroU32;

use pqc_kyber::{KYBER_CIPHERTEXTBYTES, KYBER_PUBLICKEYBYTES, KYBER_SECRETKEYBYTES};

use crate::crypto::aes::AES_256_KEY_SIZE;
use crate::crypto::aes_gcm::{AesGcmAead, AES_GCM_IV_SIZE, AES_GCM_TAG_SIZE};
use crate::crypto::p384::{P384KeyPair, P384PublicKey, P384_PUBLIC_KEY_SIZE};
use crate::crypto::rand_core::RngCore;
use crate::crypto::secret::Secret;
use crate::error::{OpenError, ReceiveError, SendError};
use crate::fragged::Fragged;
use crate::proto::*;
use crate::ratchet_state::RatchetState;
use crate::symmetric_state::SymmetricState;
use crate::ApplicationLayer;

/// Create a 96-bit AES-GCM nonce.
///
/// The primary information that we want to be contained here is the counter and the
/// packet type. The former makes this unique and the latter's inclusion authenticates
/// it as effectively AAD. Other elements of the header are either not authenticated,
/// like fragmentation info, or their authentication is implied via key exchange like
/// the key id.
pub fn nonce(packet_type: u8, counter: u64) -> [u8; AES_GCM_IV_SIZE] {
    let mut ret = [0u8; AES_GCM_IV_SIZE];
    ret[3] = packet_type;
    // Noise requires a big endian counter at the end of the Nonce
    ret[4..].copy_from_slice(&counter.to_be_bytes());
    ret
}

pub struct Zeta<App: ApplicationLayer> {
    /// An arbitrary application defined object associated with each session.
    pub application_data: App::Data,
    /// Is true if the local peer acted as Bob, the responder in the initial key exchange.
    pub was_bob: bool,

    s_remote: App::PublicKey,
    send_counter: u64,
    key_creation_counter: u64,

    key_index: bool,
    keys: [DualKeys; 2],
    ratchet_states: [RatchetState; 2],
    hk_send: Secret<AES_256_KEY_SIZE>,
    hk_recv: Secret<AES_256_KEY_SIZE>,

    resend_timer: i64,
    timeout_timer: i64,
    beta: ZsspAutomata<App>,

    counter_antireplay_window: [u64; COUNTER_WINDOW_MAX_OOO],
    defrag: [Fragged<App::IncomingPacketBuffer, MAX_FRAGMENTS>; SESSION_MAX_FRAGMENTS_OOO],
}

pub struct StateB2<App: ApplicationLayer> {
    /// Can never be Null.
    ratchet_state: RatchetState,
    kid_send: NonZeroU32,
    kid_recv: NonZeroU32,
    hk_send: Secret<AES_256_KEY_SIZE>,
    hk_recv: Secret<AES_256_KEY_SIZE>,
    e_secret: App::KeyPair,
    noise: SymmetricState<App>,
    defrag: Fragged<App::IncomingPacketBuffer, MAX_FRAGMENTS>,
}

#[derive(Default)]
pub struct DualKeys {
    send: Keys,
    recv: Keys,
}
#[derive(Default)]
pub struct Keys {
    kek: Option<Secret<AES_256_KEY_SIZE>>,
    nk: Option<Secret<AES_256_KEY_SIZE>>,
    kid: Option<NonZeroU32>,
}

enum ZsspAutomata<App: ApplicationLayer> {
    Null,
    S2,
    A1 {
        noise: SymmetricState<App>,
        e_secret: App::KeyPair,
        e1_secret: Secret<KYBER_SECRETKEYBYTES>,
        x1: Vec<u8>,
    },
    A3 {
        x3: Vec<u8>,
    },
    R1 {
        noise: SymmetricState<App>,
        e_secret: App::KeyPair,
        k1: Vec<u8>,
    },
    R2 {
        k2: Vec<u8>,
    },
    S1,
}

impl<App: ApplicationLayer> SymmetricState<App> {
    fn write_e(&mut self, rng: &mut App::Rng, packet: &mut Vec<u8>) -> App::KeyPair {
        let e_secret = App::KeyPair::generate(rng);
        let pub_key = e_secret.public_key_bytes();
        packet.extend(&pub_key);
        self.mix_hash(&pub_key);
        self.mix_key(&pub_key);
        e_secret
    }
    fn read_e(&mut self, i: &mut usize, packet: &Vec<u8>) -> Result<App::PublicKey, ReceiveError<App::IoError>> {
        let j = *i + P384_PUBLIC_KEY_SIZE;
        let pub_key = &packet[*i..j];
        self.mix_hash(pub_key);
        self.mix_key(pub_key);
        *i = j;
        App::PublicKey::from_bytes((pub_key).try_into().unwrap()).ok_or(ReceiveError::FailedAuthentication)
    }
    fn token_dh(&mut self, secret: &App::KeyPair, remote: &App::PublicKey) -> Option<()> {
        let mut ecdh = Secret::new();
        if !secret.agree(&remote, ecdh.as_mut()) {
            return None;
        }
        self.mix_key(ecdh.as_ref());
        Some(())
    }
}

pub struct Packet(u32, [u8; AES_GCM_IV_SIZE], Vec<u8>);

impl<App: ApplicationLayer> Zeta<App> {
    pub fn key_ref(&self, is_next: bool) -> &DualKeys {
        &self.keys[(self.key_index ^ is_next) as usize]
    }
    pub fn key_mut(&mut self, is_next: bool) -> &mut DualKeys {
        &mut self.keys[(self.key_index ^ is_next) as usize]
    }
    pub fn trans_null_to_a1(
        app: App,
        rng: &mut App::Rng,
        kid_recv: NonZeroU32,
        s_remote: App::PublicKey,
        application_data: App::Data,
    ) -> Result<(Self, Packet), OpenError<App::IoError>> {
        let ratchet_states = app
            .restore_by_identity(&s_remote, &application_data)
            .map_err(|e| OpenError::RatchetIoError(e))?;

        let mut noise = SymmetricState::<App>::initialize(INITIAL_H);
        let mut x1 = Vec::new();
        // Process prologue.
        let kid = kid_recv.get().to_be_bytes();
        x1.extend(&kid);
        noise.mix_hash(&kid);
        noise.mix_hash(&s_remote.to_bytes());
        // X1 process e.
        let e_secret = noise.write_e(rng, &mut x1);
        // X1 process es.
        noise.token_dh(&e_secret, &s_remote).ok_or(OpenError::InvalidPublicKey)?;
        // X1 process e1.
        let i = x1.len();
        let e1_secret = pqc_kyber::keypair(rng);
        x1.extend(&e1_secret.public);
        noise.encrypt_and_hash(nonce(PACKET_TYPE_HANDSHAKE_HELLO, 0), i, &mut x1);
        // X1 process payload.
        let i = x1.len();
        for r in &ratchet_states {
            if let Some(rf) = r.fingerprint() {
                x1.extend(rf);
            }
        }
        noise.encrypt_and_hash(nonce(PACKET_TYPE_HANDSHAKE_HELLO, 1), i, &mut x1);

        let (hk_recv, hk_send) = noise.get_ask(LABEL_HEADER_KEY);
        let c = u64::from_be_bytes(x1[x1.len() - 8..].try_into().unwrap());

        let keys = DualKeys {
            send: Keys { kek: None, nk: None, kid: None },
            recv: Keys { kek: None, nk: None, kid: Some(kid_recv) },
        };
        let current_time = app.time();
        Ok((
            Self {
                application_data,
                was_bob: false,
                s_remote,
                send_counter: 0,
                key_creation_counter: 0,
                counter_antireplay_window: std::array::from_fn(|_| 0),
                defrag: std::array::from_fn(|_| Fragged::new()),
                key_index: true,
                keys: [keys, DualKeys::default()],
                ratchet_states,
                hk_recv,
                hk_send,
                resend_timer: current_time + App::RETRY_INTERVAL_MS,
                timeout_timer: current_time + App::EXPIRATION_TIMEOUT_MS,
                beta: ZsspAutomata::A1 {
                    noise,
                    e_secret,
                    e1_secret: Secret(e1_secret.secret),
                    x1: x1.clone(),
                },
            },
            Packet(0, nonce(PACKET_TYPE_HANDSHAKE_HELLO, c), x1),
        ))
    }
    pub fn trans_null_to_b2(
        app: App,
        rng: &mut App::Rng,
        kid_gen: impl FnOnce(&mut App::Rng) -> NonZeroU32,
        s_secret: &App::KeyPair,
        c: u64,
        mut x1: Vec<u8>,
    ) -> Result<(StateB2<App>, Packet), ReceiveError<App::IoError>> {
        use ReceiveError::*;
        if !(HANDSHAKE_HELLO_MIN_SIZE..=HANDSHAKE_HELLO_MAX_SIZE).contains(&x1.len()) {
            return Err(FailedAuthentication);
        }
        if c != u64::from_be_bytes(x1[x1.len() - 8..].try_into().unwrap()) {
            return Err(FailedAuthentication);
        }
        let mut noise = SymmetricState::<App>::initialize(INITIAL_H);
        let mut i = 0;
        // Process prologue.
        let j = i + SESSION_ID_SIZE;
        noise.mix_hash(&x1[i..j]);
        let kid_send = NonZeroU32::new(u32::from_be_bytes(x1[i..j].try_into().unwrap())).ok_or(FailedAuthentication)?;
        noise.mix_hash(&s_secret.public_key_bytes());
        i = j;
        // X1 process e.
        let e_remote = noise.read_e(&mut i, &x1)?;
        // X1 process es.
        noise.token_dh(s_secret, &e_remote).ok_or(FailedAuthentication)?;
        // X1 process e1.
        let j = i + KYBER_PUBLICKEYBYTES;
        let k = j + AES_GCM_TAG_SIZE;
        let tag = x1[j..k].try_into().unwrap();
        if !noise.decrypt_and_hash(nonce(PACKET_TYPE_HANDSHAKE_HELLO, 0), &mut x1[i..j], tag) {
            return Err(FailedAuthentication);
        }
        let e1_start = i;
        let e1_end = j;
        i = k;
        // X1 process payload.
        let k = x1.len();
        let j = k - AES_GCM_TAG_SIZE;
        let tag = x1[j..k].try_into().unwrap();
        if !noise.decrypt_and_hash(nonce(PACKET_TYPE_HANDSHAKE_HELLO, 0), &mut x1[i..j], tag) {
            return Err(FailedAuthentication);
        }
        // X1 get ratchet key.
        let mut ratchet_state = RatchetState::Null;
        while i + RATCHET_SIZE <= j {
            match app.restore_by_fingerprint((&x1[i..i + RATCHET_SIZE]).try_into().unwrap()) {
                Ok(RatchetState::Null) | Ok(RatchetState::Empty) => {}
                Ok(rs) => {
                    ratchet_state = rs;
                    break;
                }
                Err(e) => return Err(RatchetIoError(e)),
            }
            i += RATCHET_SIZE;
        }
        if ratchet_state.is_null() {
            if app.hello_requires_recognized_ratchet() {
                return Err(FailedAuthentication);
            }
            ratchet_state = RatchetState::Empty;
        }
        let (hk_send, hk_recv) = noise.get_ask(LABEL_HEADER_KEY);

        let mut x2 = Vec::new();
        // X2 process e token.
        let e_secret = noise.write_e(rng, &mut x2);
        // X2 process ee token.
        noise.token_dh(&e_secret, &e_remote).ok_or(FailedAuthentication)?;
        // X2 process ekem1 token.
        let i = x2.len();
        let (ekem1, ekem1_secret) = pqc_kyber::encapsulate(&x1[e1_start..e1_end], rng)
            .map_err(|_| FailedAuthentication)
            .map(|(ct, ekem1)| (ct, Secret(ekem1)))?;
        x2.extend(ekem1);
        noise.encrypt_and_hash(nonce(PACKET_TYPE_HANDSHAKE_RESPONSE, 0), i, &mut x2);
        noise.mix_key(ekem1_secret.as_ref());
        drop(ekem1_secret);
        // X2 process psk token.
        noise.mix_key_and_hash(ratchet_state.key().unwrap());
        // X2 process payload.
        let i = x2.len();
        let kid_recv = kid_gen(rng);
        x2.extend(kid_recv.get().to_be_bytes());
        noise.encrypt_and_hash(nonce(PACKET_TYPE_HANDSHAKE_RESPONSE, 0), i, &mut x2);

        let i = x1.len();
        let mut c = 0u64.to_be_bytes();
        c[5] = x1[i - 3];
        c[6] = x1[i - 2];
        c[7] = x1[i - 1];
        let c = u64::from_be_bytes(c);
        Ok((
            StateB2 {
                ratchet_state,
                kid_send,
                kid_recv,
                hk_send,
                hk_recv,
                e_secret,
                noise,
                defrag: Fragged::new(),
            },
            Packet(kid_send.get(), nonce(PACKET_TYPE_HANDSHAKE_RESPONSE, c), x2),
        ))
    }
    pub fn trans_a1_to_a3(
        &mut self,
        app: App,
        kid: NonZeroU32,
        c: u64,
        mut x2: Vec<u8>,
        s_secret: &App::KeyPair,
        identity: &[u8],
    ) -> Result<Packet, ReceiveError<App::IoError>> {
        use ReceiveError::*;
        if HANDSHAKE_RESPONSE_SIZE != x2.len() {
            return Err(FailedAuthentication);
        }

        if Some(kid) != self.key_ref(true).recv.kid {
            return Err(FailedAuthentication);
        }
        if let ZsspAutomata::A1 { noise, e_secret, e1_secret, .. } = &self.beta {
            let mut noise = noise.clone();
            if c >= 1 << 24 || &c.to_be_bytes()[5..] != &x2[x2.len() - 3..] {
                return Err(FailedAuthentication);
            }
            let mut i = 0;
            // X2 process e token.
            let e_remote = noise.read_e(&mut i, &x2)?;
            // X2 process ee token.
            noise.token_dh(e_secret, &e_remote).ok_or(FailedAuthentication)?;
            // Noise process pattern2 ekem1 token.
            let j = i + KYBER_CIPHERTEXTBYTES;
            let k = j + AES_GCM_TAG_SIZE;
            let tag = x2[j..k].try_into().unwrap();
            if !noise.decrypt_and_hash(nonce(PACKET_TYPE_HANDSHAKE_RESPONSE, 0), &mut x2[i..j], tag) {
                return Err(FailedAuthentication);
            }
            let ekem1_secret = pqc_kyber::decapsulate(&x2[i..j], e1_secret.as_ref())
                .map(Secret)
                .map_err(|_| FailedAuthentication)?;
            noise.mix_key(ekem1_secret.as_ref());
            drop(ekem1_secret);
            i = k;
            // We attempt to decrypt the payload at most three times. First two times with
            // the ratchet key Alice remembers, and final time with a ratchet
            // key of zero if Alice allows ratchet downgrades.
            // The following code is not constant time, meaning we leak to an
            // attacker whether or not we downgraded.
            // We don't currently consider this sensitive enough information to hide.
            let j = i + SESSION_ID_SIZE;
            let k = j + AES_GCM_TAG_SIZE;
            let payload: [u8; SESSION_ID_SIZE] = x2[i..j].try_into().unwrap();
            let tag = x2[j..k].try_into().unwrap();
            // Check for which ratchet key Bob wants to use.
            let test_ratchet_key = |ratchet_key| -> Option<(NonZeroU32, SymmetricState<App>)> {
                let mut noise = noise.clone();
                let mut payload = payload.clone();
                // Noise process pattern2 psk token.
                noise.mix_key_and_hash(ratchet_key);
                // Noise process pattern2 payload.
                if !noise.decrypt_and_hash(nonce(PACKET_TYPE_HANDSHAKE_RESPONSE, 0), &mut payload, tag) {
                    return None;
                }
                NonZeroU32::new(u32::from_ne_bytes(payload)).map(|kid2| (kid2, noise))
            };
            // Check first key.
            let mut ratchet_i = 0;
            let mut result = None;
            let mut chain_len = 0;
            if let Some(key) = self.ratchet_states[0].key() {
                chain_len = self.ratchet_states[0].chain_len();
                result = test_ratchet_key(key);
            }
            // Check second key.
            if result.is_none() {
                ratchet_i = 1;
                if let Some(key) = self.ratchet_states[1].key() {
                    chain_len = self.ratchet_states[1].chain_len();
                    result = test_ratchet_key(key);
                }
            }
            // Check zero key.
            if result.is_none() && !app.initiator_disallows_downgrade() {
                chain_len = 0;
                result = test_ratchet_key(&[0u8; RATCHET_SIZE]);
                if result.is_some() {
                    // TODO: add some kind of warning callback or signal.
                }
            }

            let (kid_send, mut noise) = result.ok_or(FailedAuthentication)?;
            let mut x3 = Vec::new();

            // Noise process pattern3 s token.
            let i = x3.len();
            x3.extend(&s_secret.public_key_bytes());
            noise.encrypt_and_hash(nonce(PACKET_TYPE_HANDSHAKE_COMPLETION, 1), i, &mut x3);
            // Noise process pattern3 se token.
            noise.token_dh(&s_secret, &e_remote).ok_or(FailedAuthentication)?;
            // Noise process pattern3 payload token.
            let i = x3.len();
            x3.extend(identity);
            noise.encrypt_and_hash(nonce(PACKET_TYPE_HANDSHAKE_COMPLETION, 0), i, &mut x3);

            let (rk, rf) = noise.get_ask(LABEL_RATCHET_STATE);
            let new_ratchet_state = RatchetState::new_incr(rk, rf, chain_len);

            let ratchet_to_preserve = &self.ratchet_states[ratchet_i];
            let result = app.save_ratchet_state(
                &self.s_remote,
                &self.application_data,
                [&self.ratchet_states[0], &self.ratchet_states[1]],
                [&new_ratchet_state, ratchet_to_preserve],
            );
            if let Err(e) = result {
                return Err(RatchetIoError(e));
            }

            let (kek_recv, kek_send) = noise.get_ask(LABEL_KEX_KEY);
            let (nk_recv, nk_send) = noise.split();

            self.key_mut(true).send.kid = Some(kid_send);
            self.key_mut(true).send.kek = Some(kek_send);
            self.key_mut(true).send.nk = Some(nk_send);
            self.key_mut(true).recv.kek = Some(kek_recv);
            self.key_mut(true).recv.nk = Some(nk_recv);
            self.ratchet_states[1] = self.ratchet_states[ratchet_i].clone();
            self.ratchet_states[0] = new_ratchet_state;
            let current_time = app.time();
            self.key_creation_counter = self.send_counter;
            self.resend_timer = current_time + App::RETRY_INTERVAL_MS;
            self.timeout_timer = current_time + App::INITIAL_OFFER_TIMEOUT_MS;
            self.beta = ZsspAutomata::A3 { x3: x3.clone() };

            Ok(Packet(kid_send.get(), nonce(PACKET_TYPE_HANDSHAKE_COMPLETION, 0), x3))
        } else {
            Err(FailedAuthentication)
        }
    }
    pub fn trans_b2_to_s1(
        zeta: &StateB2<App>,
        app: App,
        kid: NonZeroU32,
        mut x3: Vec<u8>,
    ) -> Result<(Self, Packet), (ReceiveError<App::IoError>, Option<Packet>)> {
        use ReceiveError::*;
        if x3.len() < HANDSHAKE_COMPLETION_MIN_SIZE {
            return Err((FailedAuthentication, None));
        }

        if kid != zeta.kid_recv {
            return Err((FailedAuthentication, None));
        }

        let mut noise = zeta.noise.clone();
        let mut i = 0;
        // Noise process pattern3 s token.
        let j = i + P384_PUBLIC_KEY_SIZE;
        let k = j + AES_GCM_TAG_SIZE;
        let tag = x3[j..k].try_into().unwrap();
        if !noise.decrypt_and_hash(nonce(PACKET_TYPE_HANDSHAKE_COMPLETION, 1), &mut x3[i..j], tag) {
            return Err((FailedAuthentication, None));
        }
        let s_remote = App::PublicKey::from_bytes((&x3[i..j]).try_into().unwrap()).ok_or((FailedAuthentication, None))?;
        i = k;
        // Noise process pattern3 se token.
        noise.token_dh(&zeta.e_secret, &s_remote).ok_or((FailedAuthentication, None))?;
        // Noise process pattern3 payload.
        let j = i + P384_PUBLIC_KEY_SIZE;
        let k = j + AES_GCM_TAG_SIZE;
        let tag = x3[j..k].try_into().unwrap();
        if !noise.decrypt_and_hash(nonce(PACKET_TYPE_HANDSHAKE_COMPLETION, 0), &mut x3[i..j], tag) {
            return Err((FailedAuthentication, None));
        }
        let identity_start = i;
        let identity_end = j;

        let (kek_send, kek_recv) = noise.get_ask(LABEL_KEX_KEY);
        let c = INIT_COUNTER;

        let (responder_disallows_downgrade, responder_silently_rejects) = app.check_accept_session(&s_remote, &x3[identity_start..identity_end]);
        let send_reject = || {
            responder_silently_rejects.then(|| {
                let mut d = Vec::<u8>::new();
                let n = nonce(PACKET_TYPE_SESSION_REJECTED, c);
                let tag = App::Aead::encrypt_in_place(kek_send.as_ref(), n, None, &mut []);
                d.extend(&tag);
                // We just used a counter with this key, but we are not storing
                // the fact we used it in memory. This is currently ok because the
                // handshake is being dropped, so nonce reuse can't happen.
                Packet(zeta.kid_send.get(), n, d)
            })
        };
        if let Some((responder_disallows_downgrade, application_data)) = responder_disallows_downgrade {
            let result = app.restore_by_identity(&s_remote, &application_data);
            match result {
                Ok(true_ratchet_states) => {
                    let mut has_match = false;
                    for rs in &true_ratchet_states {
                        if !rs.is_null() {
                            has_match |= &zeta.ratchet_state == rs;
                        }
                    }
                    if !has_match {
                        if !responder_disallows_downgrade && zeta.ratchet_state.is_empty() {
                            // TODO: add some kind of warning callback or signal.
                        } else {
                            return Err((FailedAuthentication, send_reject()));
                        }
                    }

                    let (rk, rf) = noise.get_ask(LABEL_RATCHET_STATE);
                    // We must make sure the ratchet key is saved before we transition.
                    let new_ratchet_state = RatchetState::new_incr(rk, rf, zeta.ratchet_state.chain_len());
                    let result = app.save_ratchet_state(
                        &s_remote,
                        &application_data,
                        [&true_ratchet_states[0], &true_ratchet_states[1]],
                        [&new_ratchet_state, &RatchetState::Null],
                    );
                    if let Err(e) = result {
                        return Err((RatchetIoError(e), None));
                    }

                    let (nk1, nk2) = noise.split();
                    let keys = DualKeys {
                        send: Keys { kek: Some(kek_send), nk: Some(nk1), kid: Some(zeta.kid_send) },
                        recv: Keys { kek: Some(kek_recv), nk: Some(nk2), kid: Some(zeta.kid_recv) },
                    };
                    let current_time = app.time();
                    let session = Self {
                        application_data,
                        was_bob: true,
                        s_remote,
                        send_counter: INIT_COUNTER + 1,
                        key_creation_counter: INIT_COUNTER + 1,
                        key_index: false,
                        keys: [keys, DualKeys::default()],
                        ratchet_states: [new_ratchet_state, RatchetState::Null],
                        hk_send: zeta.hk_send.clone(),
                        hk_recv: zeta.hk_recv.clone(),
                        resend_timer: current_time + App::RETRY_INTERVAL_MS,
                        timeout_timer: current_time + App::EXPIRATION_TIMEOUT_MS,
                        beta: ZsspAutomata::S1,
                        counter_antireplay_window: std::array::from_fn(|_| 0),
                        defrag: std::array::from_fn(|_| Fragged::new()),
                    };

                    let mut c1 = Vec::new();
                    let tag = App::Aead::encrypt_in_place(
                        session.key_ref(false).send.kek.as_ref().unwrap().as_ref(),
                        nonce(PACKET_TYPE_KEY_CONFIRM, c),
                        None,
                        &mut [],
                    );
                    c1.extend(&tag);

                    Ok((session, Packet(zeta.kid_send.get(), nonce(PACKET_TYPE_KEY_CONFIRM, c), c1)))
                }
                Err(e) => Err((RatchetIoError(e), None)),
            }
        } else {
            Err((FailedAuthentication, send_reject()))
        }
    }
    pub fn trans_to_s2(
        &mut self,
        app: App,
        rng: &mut App::Rng,
        kid: NonZeroU32,
        n: [u8; AES_GCM_IV_SIZE],
        c1: Vec<u8>,
    ) -> Result<Packet, ReceiveError<App::IoError>> {
        use ReceiveError::*;
        if c1.len() != KEY_CONFIRMATION_SIZE {
            return Err(FailedAuthentication);
        }

        let is_other = if Some(kid) == self.key_ref(true).recv.kid {
            true
        } else if Some(kid) == self.key_ref(false).recv.kid {
            false
        } else {
            return Err(OutOfSequence);
        };

        let tag = c1[..].try_into().unwrap();
        if !App::Aead::decrypt_in_place(
            self.key_ref(is_other).recv.kek.as_ref().ok_or(OutOfSequence)?.as_ref(),
            n,
            None,
            &mut [],
            tag,
        ) {
            return Err(FailedAuthentication);
        }

        if is_other {
            if let ZsspAutomata::A3 { .. } | ZsspAutomata::R2 { .. } = &self.beta {
                let result = if !self.ratchet_states[1].is_null() {
                    app.save_ratchet_state(
                        &self.s_remote,
                        &self.application_data,
                        [&self.ratchet_states[0], &self.ratchet_states[1]],
                        [&self.ratchet_states[0], &RatchetState::Null],
                    )
                } else {
                    Ok(())
                };
                if let Err(e) = result {
                    return Err(RatchetIoError(e));
                }

                self.ratchet_states[1] = RatchetState::Null;
                self.key_index ^= true;
                self.timeout_timer = app.time() + (rng.next_u64() as i64 % App::REKEY_AFTER_TIME_MAX_JITTER_MS);
                self.resend_timer = i64::MAX;
                self.beta = ZsspAutomata::S2;
            }
        }
        let mut c2 = Vec::new();

        let c = self.send_counter;
        self.send_counter += 1;
        let n = nonce(PACKET_TYPE_ACK, c);
        let tag = App::Aead::encrypt_in_place(self.key_ref(false).send.kek.as_ref().ok_or(OutOfSequence)?.as_ref(), n, None, &mut []);
        c2.extend(&tag);

        Ok(Packet(self.key_ref(false).send.kid.unwrap().get(), n, c2))
    }
    pub fn trans_s1_to_s2(
        &mut self,
        app: App,
        rng: &mut App::Rng,
        kid: NonZeroU32,
        n: [u8; AES_GCM_IV_SIZE],
        c2: Vec<u8>,
    ) -> Result<(), ReceiveError<App::IoError>> {
        use ReceiveError::*;
        if c2.len() != ACKNOWLEDGEMENT_SIZE {
            return Err(FailedAuthentication);
        }

        if Some(kid) != self.key_ref(false).recv.kid {
            return Err(OutOfSequence);
        }
        if !matches!(&self.beta, ZsspAutomata::S1) {
            return Err(OutOfSequence);
        }

        let tag = c2[..].try_into().unwrap();
        if !App::Aead::decrypt_in_place(self.key_ref(false).recv.kek.as_ref().unwrap().as_ref(), n, None, &mut [], tag) {
            return Err(FailedAuthentication);
        }
        self.timeout_timer = app.time() + (rng.next_u64() as i64 % App::REKEY_AFTER_TIME_MAX_JITTER_MS);
        self.resend_timer = i64::MAX;
        self.beta = ZsspAutomata::S2;
        Ok(())
    }
    pub fn trans_s2_to_r1(
        &mut self,
        app: App,
        rng: &mut App::Rng,
        kid_recv: NonZeroU32,
        s_secret: &App::KeyPair,
    ) -> Result<Packet, ReceiveError<App::IoError>> {
        use ReceiveError::*;
        if !matches!(&self.beta, ZsspAutomata::S2) {
            return Err(OutOfSequence);
        }

        let mut k1 = Vec::new();
        let mut noise = SymmetricState::initialize(INITIAL_H_REKEY);
        // Noise process prologue.
        noise.mix_hash(&s_secret.public_key_bytes());
        noise.mix_hash(&self.s_remote.to_bytes());
        // Noise process pattern1 psk0 token.
        noise.mix_key_and_hash(self.ratchet_states[0].key().unwrap());
        // Noise process pattern1 e token.
        let e_secret = noise.write_e(rng, &mut k1);
        // Noise process pattern1 es token.
        noise.token_dh(&e_secret, &self.s_remote).ok_or(FailedAuthentication)?;
        // Noise process pattern1 ss token.
        noise.token_dh(s_secret, &self.s_remote).ok_or(FailedAuthentication)?;
        // Noise process pattern1 payload token.
        let i = k1.len();
        k1.extend(&kid_recv.get().to_be_bytes());
        noise.encrypt_and_hash(nonce(PACKET_TYPE_REKEY_INIT, 0), i, &mut k1);

        let c = self.send_counter;
        self.send_counter += 1;
        let n = nonce(PACKET_TYPE_REKEY_INIT, c);
        let tag = App::Aead::encrypt_in_place(self.key_ref(false).send.kek.as_ref().unwrap().as_ref(), n, None, &mut k1);
        k1.extend(&tag);

        self.key_mut(true).recv.kid = Some(kid_recv);
        let current_time = app.time();
        self.timeout_timer = current_time + App::EXPIRATION_TIMEOUT_MS;
        self.resend_timer = current_time + App::RETRY_INTERVAL_MS;
        self.beta = ZsspAutomata::R1 { noise, e_secret, k1: k1.clone() };

        Ok(Packet(self.key_ref(false).send.kid.unwrap().get(), n, k1))
    }
    pub fn trans_to_r2(
        &mut self,
        app: App,
        rng: &mut App::Rng,
        kid_recv: NonZeroU32,
        s_secret: &App::KeyPair,
        kid: NonZeroU32,
        n: [u8; AES_GCM_IV_SIZE],
        mut k1: Vec<u8>,
    ) -> Result<Packet, ReceiveError<App::IoError>> {
        use ReceiveError::*;
        if k1.len() != REKEY_SIZE {
            return Err(FailedAuthentication);
        }

        if Some(kid) != self.key_ref(false).recv.kid {
            return Err(OutOfSequence);
        }
        let should_rekey_as_bob = match &self.beta {
            ZsspAutomata::S2 { .. } => true,
            ZsspAutomata::R1 { .. } => self.was_bob,
            _ => false,
        };
        if !should_rekey_as_bob {
            return Err(OutOfSequence);
        }

        let i = k1.len() - AES_GCM_TAG_SIZE;
        let tag = k1[i..].try_into().unwrap();
        if !App::Aead::decrypt_in_place(self.key_ref(false).recv.kek.as_ref().unwrap().as_ref(), n, None, &mut k1[..i], tag) {
            return Err(FailedAuthentication);
        }

        let mut i = 0;
        let mut noise = SymmetricState::<App>::initialize(INITIAL_H_REKEY);
        // Noise process prologue.
        noise.mix_hash(&self.s_remote.to_bytes());
        noise.mix_hash(&s_secret.public_key_bytes());
        // Noise process pattern1 psk0 token.
        noise.mix_key_and_hash(self.ratchet_states[0].key().unwrap());
        // Noise process pattern1 e token.
        let e_remote = noise.read_e(&mut i, &k1)?;
        // Noise process pattern1 es token.
        noise.token_dh(s_secret, &e_remote).ok_or(FailedAuthentication)?;
        // Noise process pattern1 ss token.
        noise.token_dh(s_secret, &self.s_remote).ok_or(FailedAuthentication)?;
        // Noise process pattern1 payload.
        let j = i + SESSION_ID_SIZE;
        let k = j + AES_GCM_TAG_SIZE;
        let tag = k1[j..k].try_into().unwrap();
        if !noise.decrypt_and_hash(nonce(PACKET_TYPE_REKEY_INIT, 0), &mut k1[i..j], tag) {
            return Err(FailedAuthentication);
        }
        let kid_send = NonZeroU32::new(u32::from_be_bytes(k1[i..j].try_into().unwrap())).ok_or(FailedAuthentication)?;

        let mut k2 = Vec::new();
        // Noise process pattern2 e token.
        let e_secret = noise.write_e(rng, &mut k2);
        // Noise process pattern2 ee token.
        noise.token_dh(&e_secret, &e_remote).ok_or(FailedAuthentication)?;
        // Noise process pattern2 se token.
        noise.token_dh(&s_secret, &e_remote).ok_or(FailedAuthentication)?;
        // Noise process pattern2 payload.
        let i = k2.len();
        k2.extend(&kid_recv.get().to_be_bytes());
        noise.encrypt_and_hash(nonce(PACKET_TYPE_REKEY_COMPLETE, 0), i, &mut k2);

        let c = self.send_counter;
        self.send_counter += 1;
        let n = nonce(PACKET_TYPE_REKEY_INIT, c);
        let tag = App::Aead::encrypt_in_place(self.key_ref(false).send.kek.as_ref().unwrap().as_ref(), n, None, &mut k2);
        k2.extend(&tag);

        let (rk, rf) = noise.get_ask(LABEL_RATCHET_STATE);
        let new_ratchet_state = RatchetState::new_incr(rk, rf, self.ratchet_states[0].chain_len());
        let result = app.save_ratchet_state(
            &self.s_remote,
            &self.application_data,
            [&self.ratchet_states[0], &self.ratchet_states[1]],
            [&new_ratchet_state, &self.ratchet_states[0]],
        );
        if let Err(e) = result {
            return Err(RatchetIoError(e));
        }
        let (kek_send, kek_recv) = noise.get_ask(LABEL_KEX_KEY);
        let (nk_send, nk_recv) = noise.split();

        self.key_mut(true).send.kid = Some(kid_send);
        self.key_mut(true).send.kek = Some(kek_send);
        self.key_mut(true).send.nk = Some(nk_send);
        self.key_mut(true).recv.kid = Some(kid_recv);
        self.key_mut(true).recv.kek = Some(kek_recv);
        self.key_mut(true).recv.nk = Some(nk_recv);
        self.ratchet_states[1] = self.ratchet_states[0].clone();
        self.ratchet_states[0] = new_ratchet_state;
        let current_time = app.time();
        self.key_creation_counter = self.send_counter;
        self.timeout_timer = current_time + App::EXPIRATION_TIMEOUT_MS;
        self.resend_timer = current_time + App::RETRY_INTERVAL_MS;
        self.beta = ZsspAutomata::R2 { k2: k2.clone() };

        Ok(Packet(self.key_ref(false).send.kid.unwrap().get(), n, k2))
    }
    pub fn trans_r1_to_s1(
        &mut self,
        app: App,
        kid: NonZeroU32,
        n: [u8; AES_GCM_IV_SIZE],
        mut k2: Vec<u8>,
    ) -> Result<Packet, ReceiveError<App::IoError>> {
        use ReceiveError::*;
        if k2.len() != REKEY_SIZE {
            return Err(FailedAuthentication);
        }

        if Some(kid) != self.key_ref(false).recv.kid {
            return Err(OutOfSequence);
        }
        if let ZsspAutomata::R1 { noise, e_secret, .. } = &self.beta {
            let i = k2.len() - AES_GCM_TAG_SIZE;
            let tag = k2[i..].try_into().unwrap();
            if !App::Aead::decrypt_in_place(self.key_ref(false).recv.kek.as_ref().unwrap().as_ref(), n, None, &mut k2[..i], tag) {
                return Err(FailedAuthentication);
            }

            let mut noise = noise.clone();
            let mut i = 0;
            // Noise process pattern2 e token.
            let e_remote = noise.read_e(&mut i, &k2)?;
            // Noise process pattern2 ee token.
            noise.token_dh(e_secret, &e_remote).ok_or(FailedAuthentication)?;
            // Noise process pattern2 se token.
            noise.token_dh(e_secret, &self.s_remote).ok_or(FailedAuthentication)?;
            // Noise process pattern2 payload.
            let j = i + SESSION_ID_SIZE;
            let k = j + AES_GCM_TAG_SIZE;
            let tag = k2[j..k].try_into().unwrap();
            if !noise.decrypt_and_hash(nonce(PACKET_TYPE_REKEY_INIT, 0), &mut k2[i..j], tag) {
                return Err(FailedAuthentication);
            }
            let kid_send = NonZeroU32::new(u32::from_be_bytes(k2[i..j].try_into().unwrap())).ok_or(FailedAuthentication)?;

            let (rk, rf) = noise.get_ask(LABEL_RATCHET_STATE);
            let new_ratchet_state = RatchetState::new_incr(rk, rf, self.ratchet_states[0].chain_len());
            let result = app.save_ratchet_state(
                &self.s_remote,
                &self.application_data,
                [&self.ratchet_states[0], &self.ratchet_states[1]],
                [&new_ratchet_state, &RatchetState::Null],
            );
            if let Err(e) = result {
                return Err(RatchetIoError(e));
            }
            let (kek_recv, kek_send) = noise.get_ask(LABEL_KEX_KEY);
            let (nk_recv, nk_send) = noise.split();

            self.key_mut(true).send.kid = Some(kid_send);
            self.key_mut(true).send.kek = Some(kek_send);
            self.key_mut(true).send.nk = Some(nk_send);
            self.key_mut(true).recv.kek = Some(kek_recv);
            self.key_mut(true).recv.nk = Some(nk_recv);
            self.ratchet_states[0] = new_ratchet_state;
            self.key_index ^= true;
            let current_time = app.time();
            self.key_creation_counter = self.send_counter;
            self.timeout_timer = current_time + App::EXPIRATION_TIMEOUT_MS;
            self.resend_timer = current_time + App::RETRY_INTERVAL_MS;
            self.beta = ZsspAutomata::S1;

            let mut c1 = Vec::new();
            let c = self.send_counter;
            self.send_counter += 1;
            let n = nonce(PACKET_TYPE_KEY_CONFIRM, c);
            let tag = App::Aead::encrypt_in_place(self.key_ref(false).send.kek.as_ref().unwrap().as_ref(), n, None, &mut []);
            c1.extend(&tag);

            Ok(Packet(self.key_ref(false).send.kid.unwrap().get(), n, c1))
        } else {
            Err(OutOfSequence)
        }
    }
    pub fn send(&mut self, mut payload: Vec<u8>) -> Result<Packet, SendError> {
        use SendError::*;
        if matches!(&self.beta, ZsspAutomata::Null) {
            return Err(SessionExpired);
        }
        if !matches!(
            &self.beta,
            ZsspAutomata::S1 | ZsspAutomata::S2 | ZsspAutomata::R1 { .. } | ZsspAutomata::R2 { .. }
        ) {
            return Err(SessionNotEstablished);
        }
        let c = self.send_counter;
        if c >= self.key_creation_counter + App::EXPIRE_AFTER_USES {
            self.expire();
        } else if c >= self.key_creation_counter + App::REKEY_AFTER_USES {
            self.timeout_timer = i64::MIN;
        }
        self.send_counter += 1;

        let n = nonce(PACKET_TYPE_DATA, c);
        let tag = App::Aead::encrypt_in_place(self.key_ref(false).send.nk.as_ref().unwrap().as_ref(), n, None, &mut payload);
        payload.extend(&tag);

        Ok(Packet(self.key_ref(false).send.kid.unwrap().get(), n, payload))
    }
    pub fn recv(&self, kid: NonZeroU32, n: [u8; AES_GCM_IV_SIZE], mut payload: Vec<u8>) -> Result<Vec<u8>, ReceiveError<App::IoError>> {
        use ReceiveError::*;
        if payload.len() < AES_GCM_TAG_SIZE {
            return Err(FailedAuthentication);
        }

        let is_other = if Some(kid) == self.key_ref(true).recv.kid {
            true
        } else if Some(kid) == self.key_ref(false).recv.kid {
            false
        } else {
            return Err(OutOfSequence);
        };

        let i = payload.len() - AES_GCM_TAG_SIZE;
        let tag = payload[i..].try_into().unwrap();
        if !App::Aead::decrypt_in_place(
            self.key_ref(is_other).recv.nk.as_ref().ok_or(OutOfSequence)?.as_ref(),
            n,
            None,
            &mut payload[..i],
            tag,
        ) {
            return Err(FailedAuthentication);
        }

        Ok(payload)
    }
    pub fn expire(&mut self) {
        self.beta = ZsspAutomata::Null;
    }
}
