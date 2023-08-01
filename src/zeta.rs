use rand_core::RngCore;
use std::collections::HashMap;
use std::num::NonZeroU32;
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex, Weak};

use crate::challenge::{gen_null_response, respond_to_challenge_in_place};
use crate::context::{ContextInner, SessionMap};
use crate::fragmentation::DefragBuffer;
use crate::proto::*;
use crate::ratchet_state::RatchetState;
use crate::result::{FaultType, OpenError, ReceiveError, ReceiveOk, SendError};
use crate::symmetric_state::SymmetricState;
use crate::ApplicationLayer;
#[cfg(feature = "logging")]
use crate::LogEvent::*;
use crate::{byzantine_fault, crypto::*, log};

/// Create a 96-bit AES-GCM nonce.
///
/// The primary information that we want to be contained here is the counter and the
/// packet type. The former makes this unique and the latter's inclusion authenticates
/// it as effectively AAD. Other elements of the header are either not authenticated,
/// like fragmentation info, or their authentication is implied via key exchange like
/// the key id.
pub(crate) fn to_nonce(packet_type: u8, counter: u64) -> [u8; AES_GCM_IV_SIZE] {
    let mut ret = [0u8; AES_GCM_IV_SIZE];
    ret[3] = packet_type;
    // Noise requires a big endian counter at the end of the Nonce
    ret[4..].copy_from_slice(&counter.to_be_bytes());
    ret
}
pub(crate) fn from_nonce(n: &[u8]) -> (u8, u64) {
    assert!(n.len() >= PACKET_NONCE_SIZE);
    let c_start = n.len() - 8;
    (n[c_start - 1], u64::from_be_bytes(n[c_start..].try_into().unwrap()))
}

pub(crate) struct Zeta<App: ApplicationLayer> {
    ctx: Weak<ContextInner<App>>,
    /// An arbitrary application defined object associated with each session.
    pub application_data: App::Data,
    /// Is true if the local peer acted as Bob, the responder in the initial key exchange.
    pub was_bob: bool,

    s_remote: App::PublicKey,
    send_counter: u64,
    key_creation_counter: u64,

    key_index: bool,
    keys: [DuplexKey; 2],
    ratchet_states: [RatchetState; 2],
    pub hk_send: Secret<AES_256_KEY_SIZE>,

    resend_timer: i64,
    timeout_timer: i64,
    pub beta: ZsspAutomata<App>,

    pub counter_antireplay_window: [u64; COUNTER_WINDOW_MAX_OOO],
    pub defrag: DefragBuffer,
}
pub struct Session<App: ApplicationLayer>(pub(crate) Mutex<Zeta<App>>);

pub(crate) struct StateB2<App: ApplicationLayer> {
    /// Can never be Null.
    ratchet_state: RatchetState,
    kid_send: NonZeroU32,
    pub kid_recv: NonZeroU32,
    pub hk_send: Secret<AES_256_KEY_SIZE>,
    e_secret: App::KeyPair,
    noise: SymmetricState<App>,
    pub defrag: DefragBuffer,
}

#[derive(Default)]
pub(crate) struct DuplexKey {
    send: Keys,
    recv: Keys,
}

#[derive(Default)]
pub(crate) struct Keys {
    kek: Option<Secret<AES_256_KEY_SIZE>>,
    nk: Option<Secret<AES_256_KEY_SIZE>>,
    kid: Option<NonZeroU32>,
}

#[derive(Clone)]
pub(crate) struct Packet(pub u32, pub [u8; AES_GCM_IV_SIZE], pub Vec<u8>);

#[derive(Clone)]
pub(crate) struct StateA1<App: ApplicationLayer> {
    noise: SymmetricState<App>,
    e_secret: App::KeyPair,
    e1_secret: App::Kem,
    identity: Vec<u8>,
    packet: Packet,
}

pub(crate) enum ZsspAutomata<App: ApplicationLayer> {
    Null,
    A1(StateA1<App>),
    A3 {
        identity: Vec<u8>,
        packet: Packet,
    },
    S1,
    S2,
    R1 {
        noise: SymmetricState<App>,
        e_secret: App::KeyPair,
        k1: Vec<u8>,
    },
    R2 {
        k2: Vec<u8>,
    },
}

impl<App: ApplicationLayer> SymmetricState<App> {
    fn write_e(&mut self, rng: &Mutex<App::Rng>, packet: &mut Vec<u8>) -> App::KeyPair {
        let e_secret = App::KeyPair::generate(rng.lock().unwrap().deref_mut());
        let pub_key = e_secret.public_key_bytes();
        packet.extend(&pub_key);
        self.mix_hash(&pub_key);
        self.mix_key(&pub_key);
        e_secret
    }
    fn read_e(&mut self, i: &mut usize, packet: &Vec<u8>) -> Option<App::PublicKey> {
        let j = *i + P384_PUBLIC_KEY_SIZE;
        let pub_key = &packet[*i..j];
        self.mix_hash(pub_key);
        self.mix_key(pub_key);
        *i = j;
        App::PublicKey::from_bytes((pub_key).try_into().unwrap())
    }
    fn mix_dh(&mut self, secret: &App::KeyPair, remote: &App::PublicKey) -> Option<()> {
        if let Some(ecdh) = secret.agree(&remote).map(Secret) {
            self.mix_key(ecdh.as_ref());
            Some(())
        } else {
            None
        }
    }
}

/// Generate a random local key id that is currently unused.
fn gen_kid<T>(session_map: &HashMap<NonZeroU32, T>, rng: &mut impl RngCore) -> NonZeroU32 {
    loop {
        if let Some(kid) = NonZeroU32::new(rng.next_u32()) {
            if !session_map.contains_key(&kid) {
                return kid;
            }
        }
    }
}

impl<App: ApplicationLayer> Zeta<App> {
    pub(crate) fn check_counter_window(&self, c: u64) -> bool {
        let slot = &self.counter_antireplay_window[c as usize % self.counter_antireplay_window.len()];
        let adj_counter = c.saturating_add(1);
        *slot < adj_counter && adj_counter <= *slot + COUNTER_WINDOW_MAX_SKIP_AHEAD
    }
    /// Updating the counter window should be the last authentication step to ensure
    /// an attacker cannot intentionally waste a peer's counters on fake packets.
    pub(crate) fn update_counter_window(&mut self, c: u64) -> bool {
        let slot = &mut self.counter_antireplay_window[c as usize % self.counter_antireplay_window.len()];
        let adj_counter = c.saturating_add(1);
        if *slot < adj_counter && adj_counter <= *slot + COUNTER_WINDOW_MAX_SKIP_AHEAD {
            *slot = adj_counter;
            true
        } else {
            false
        }
    }
    fn key_ref(&self, is_next: bool) -> &DuplexKey {
        &self.keys[(self.key_index ^ is_next) as usize]
    }
    fn key_mut(&mut self, is_next: bool) -> &mut DuplexKey {
        &mut self.keys[(self.key_index ^ is_next) as usize]
    }
    fn expire(&mut self) {
        self.resend_timer = i64::MAX;
        self.timeout_timer = i64::MAX;
        self.beta = ZsspAutomata::Null;
        if let Some(ctx) = self.ctx.upgrade() {
            let mut session_map = ctx.session_map.lock().unwrap();
            let mut sessions = ctx.sessions.lock().unwrap();
            for key in &self.keys {
                if let Some(kid_recv) = key.recv.kid {
                    if let Some(weak) = session_map.remove(&kid_recv) {
                        sessions.remove(&weak.as_ptr());
                    }
                }
            }
        }
    }
    pub(crate) fn next_timer(&self) -> i64 {
        self.timeout_timer.min(self.resend_timer)
    }
}

fn create_a1_state<App: ApplicationLayer>(
    rng: &Mutex<App::Rng>,
    s_remote: &App::PublicKey,
    kid_recv: NonZeroU32,
    ratchet_states: &[RatchetState; 2],
    identity: Vec<u8>,
) -> Option<StateA1<App>> {
    let mut noise = SymmetricState::<App>::initialize(PROTOCOL_NAME_NOISE_XK);
    let mut x1 = Vec::new();
    // Process prologue.
    let kid = kid_recv.get().to_be_bytes();
    x1.extend(&kid);
    noise.mix_hash(&kid);
    noise.mix_hash(&s_remote.to_bytes());
    // X1 process e.
    let e_secret = noise.write_e(rng, &mut x1);
    // X1 process es.
    noise.mix_dh(&e_secret, s_remote)?;
    // X1 process e1.
    let i = x1.len();
    let (e1_secret, e1_public) = App::Kem::generate(rng.lock().unwrap().deref_mut());
    x1.extend(&e1_public);
    noise.encrypt_and_hash_in_place(to_nonce(PACKET_TYPE_HANDSHAKE_HELLO, 0), i, &mut x1);
    // X1 process payload.
    let i = x1.len();
    for r in ratchet_states {
        if let Some(rf) = r.fingerprint() {
            x1.extend(rf);
        }
    }
    noise.encrypt_and_hash_in_place(to_nonce(PACKET_TYPE_HANDSHAKE_HELLO, 1), i, &mut x1);

    let c = u64::from_be_bytes(x1[x1.len() - 8..].try_into().unwrap());

    x1.extend(&gen_null_response(rng.lock().unwrap().deref_mut()));
    Some(StateA1 {
        noise,
        e_secret,
        e1_secret,
        identity,
        packet: Packet(0, to_nonce(PACKET_TYPE_HANDSHAKE_HELLO, c), x1),
    })
}
pub(crate) fn trans_to_a1<App: ApplicationLayer>(
    app: App,
    ctx: &Arc<ContextInner<App>>,
    s_remote: App::PublicKey,
    application_data: App::Data,
    identity: Vec<u8>,
    send: impl FnOnce(&Packet),
) -> Result<Arc<Session<App>>, OpenError<App::IoError>> {
    let ratchet_states = app
        .restore_by_identity(&s_remote, &application_data)
        .map_err(|e| OpenError::RatchetIoError(e))?;

    let mut session_map = ctx.session_map.lock().unwrap();
    let kid_recv = gen_kid(session_map.deref(), ctx.rng.lock().unwrap().deref_mut());

    let a1 = create_a1_state(&ctx.rng, &s_remote, kid_recv, &ratchet_states, identity).ok_or(OpenError::InvalidPublicKey)?;
    let packet = a1.packet.clone();

    let (hk_recv, hk_send) = a1.noise.get_ask(LABEL_HEADER_KEY);

    let current_time = app.time();
    let mut zeta = Zeta {
        ctx: Arc::downgrade(ctx),
        application_data,
        was_bob: false,
        s_remote,
        send_counter: 0,
        key_creation_counter: 0,
        counter_antireplay_window: std::array::from_fn(|_| 0),
        defrag: DefragBuffer::new(Some(hk_recv)),
        key_index: true,
        keys: [DuplexKey::default(), DuplexKey::default()],
        ratchet_states,
        hk_send,
        resend_timer: current_time + App::SETTINGS.resend_time,
        timeout_timer: current_time + App::SETTINGS.initial_offer_timeout,
        beta: ZsspAutomata::A1(a1),
    };
    zeta.key_mut(true).recv.kid = Some(kid_recv);

    let session = Arc::new(Session(Mutex::new(zeta)));
    session_map.insert(kid_recv, Arc::downgrade(&session));
    ctx.sessions.lock().unwrap().insert(Arc::as_ptr(&session), Arc::downgrade(&session));

    send(&packet);

    Ok(session)
}
pub(crate) fn respond_to_challenge<App: ApplicationLayer>(zeta: &mut Zeta<App>, rng: &Mutex<App::Rng>, challenge: &[u8; CHALLENGE_SIZE]) {
    if let ZsspAutomata::A1(StateA1 { packet: Packet(_, _, x1), .. }) = &mut zeta.beta {
        let response_start = x1.len() - CHALLENGE_SIZE;
        respond_to_challenge_in_place::<App::Rng, App::Hash>(
            rng.lock().unwrap().deref_mut(),
            challenge,
            (&mut x1[response_start..]).try_into().unwrap(),
        );
    }
}
pub(crate) fn recv_x1_trans<App: ApplicationLayer>(
    app: &App,
    ctx: &ContextInner<App>,
    n: [u8; AES_GCM_IV_SIZE],
    mut x1: Vec<u8>,
    send: impl FnOnce(&Packet, &[u8; AES_256_KEY_SIZE]),
) -> Result<(), ReceiveError<App::IoError>> {
    use FaultType::*;
    if !(HANDSHAKE_HELLO_MIN_SIZE..=HANDSHAKE_HELLO_MAX_SIZE).contains(&x1.len()) {
        return Err(byzantine_fault!(InvalidPacket, true));
    }
    if &n[AES_GCM_IV_SIZE - 8..] != &x1[x1.len() - 8..] {
        return Err(byzantine_fault!(FailedAuth, true));
    }
    let mut noise = SymmetricState::<App>::initialize(PROTOCOL_NAME_NOISE_XK);
    let mut i = 0;
    // Process prologue.
    let j = i + KID_SIZE;
    noise.mix_hash(&x1[i..j]);
    let kid_send = NonZeroU32::new(u32::from_be_bytes(x1[i..j].try_into().unwrap())).ok_or(byzantine_fault!(InvalidPacket, true))?;
    noise.mix_hash(&ctx.s_secret.public_key_bytes());
    i = j;
    // X1 process e.
    let e_remote = noise.read_e(&mut i, &x1).ok_or(byzantine_fault!(FailedAuth, true))?;
    // X1 process es.
    noise.mix_dh(&ctx.s_secret, &e_remote).ok_or(byzantine_fault!(FailedAuth, true))?;
    // X1 process e1.
    let j = i + KYBER_PUBLIC_KEY_SIZE;
    let k = j + AES_GCM_TAG_SIZE;
    let tag = x1[j..k].try_into().unwrap();
    if !noise.decrypt_and_hash_in_place(to_nonce(PACKET_TYPE_HANDSHAKE_HELLO, 0), &mut x1[i..j], tag) {
        return Err(byzantine_fault!(FailedAuth, true));
    }
    let e1_start = i;
    let e1_end = j;
    i = k;
    // X1 process payload.
    let k = x1.len();
    let j = k - AES_GCM_TAG_SIZE;
    let tag = x1[j..k].try_into().unwrap();
    if !noise.decrypt_and_hash_in_place(to_nonce(PACKET_TYPE_HANDSHAKE_HELLO, 1), &mut x1[i..j], tag) {
        return Err(byzantine_fault!(FailedAuth, true));
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
            Err(e) => return Err(ReceiveError::RatchetIoError(e)),
        }
        i += RATCHET_SIZE;
    }
    if ratchet_state.is_null() {
        if app.hello_requires_recognized_ratchet() {
            return Err(byzantine_fault!(FailedAuth, true));
        }
        ratchet_state = RatchetState::Empty;
    }
    let (hk_send, hk_recv) = noise.get_ask(LABEL_HEADER_KEY);

    let mut x2 = Vec::new();
    // X2 process e token.
    let e_secret = noise.write_e(&ctx.rng, &mut x2);
    // X2 process ee token.
    noise.mix_dh(&e_secret, &e_remote).ok_or(byzantine_fault!(FailedAuth, true))?;
    // X2 process ekem1 token.
    let i = x2.len();
    let (ekem1, ekem1_secret) = App::Kem::encapsulate(ctx.rng.lock().unwrap().deref_mut(), (&x1[e1_start..e1_end]).try_into().unwrap())
        .map(|(ct, secret)| (ct, Secret(secret)))
        .ok_or(byzantine_fault!(FailedAuth, true))?;
    x2.extend(ekem1);
    noise.encrypt_and_hash_in_place(to_nonce(PACKET_TYPE_HANDSHAKE_RESPONSE, 0), i, &mut x2);
    noise.mix_key(ekem1_secret.as_ref());
    drop(ekem1_secret);
    // X2 process psk token.
    noise.mix_key_and_hash(ratchet_state.key().unwrap());
    // X2 process payload.
    let session_map = ctx.session_map.lock().unwrap();
    let kid_recv = gen_kid(session_map.deref(), ctx.rng.lock().unwrap().deref_mut());

    let i = x2.len();
    x2.extend(kid_recv.get().to_be_bytes());
    noise.encrypt_and_hash_in_place(to_nonce(PACKET_TYPE_HANDSHAKE_RESPONSE, 0), i, &mut x2);

    let i = x2.len();
    let mut c = 0u64.to_be_bytes();
    c[5] = x2[i - 3];
    c[6] = x2[i - 2];
    c[7] = x2[i - 1];
    let c = u64::from_be_bytes(c);

    ctx.b2_map.lock().unwrap().insert(
        kid_recv,
        StateB2 {
            ratchet_state,
            kid_send,
            kid_recv,
            hk_send: hk_send.clone(),
            e_secret,
            noise,
            defrag: DefragBuffer::new(Some(hk_recv)),
        },
    );

    send(&Packet(kid_send.get(), to_nonce(PACKET_TYPE_HANDSHAKE_RESPONSE, c), x2), hk_send.as_ref());
    Ok(())
}
pub(crate) fn recv_x2_trans<App: ApplicationLayer>(
    zeta: &mut Zeta<App>,
    app: &App,
    kid: NonZeroU32,
    n: [u8; AES_GCM_IV_SIZE],
    mut x2: Vec<u8>,
    s_secret: &App::KeyPair,
    send: impl FnOnce(&Packet, &[u8; AES_256_KEY_SIZE]),
) -> Result<ReceiveOk<App>, ReceiveError<App::IoError>> {
    use FaultType::*;

    if HANDSHAKE_RESPONSE_SIZE != x2.len() {
        return Err(byzantine_fault!(InvalidPacket, true));
    }
    if Some(kid) != zeta.key_ref(true).recv.kid {
        return Err(byzantine_fault!(UnknownLocalKeyId, true));
    }
    let (_, c) = from_nonce(&n);
    if c >= COUNTER_WINDOW_MAX_SKIP_AHEAD || &n[AES_GCM_IV_SIZE - 3..] != &x2[x2.len() - 3..] {
        return Err(byzantine_fault!(FailedAuth, true));
    }
    if let ZsspAutomata::A1(StateA1 { noise, e_secret, e1_secret, identity, .. }) = &zeta.beta {
        let mut noise = noise.clone();
        let mut i = 0;
        // X2 process e token.
        let e_remote = noise.read_e(&mut i, &x2).ok_or(byzantine_fault!(FailedAuth, true))?;
        // X2 process ee token.
        noise.mix_dh(e_secret, &e_remote).ok_or(byzantine_fault!(FailedAuth, true))?;
        // Noise process pattern2 ekem1 token.
        let j = i + KYBER_CIPHERTEXT_SIZE;
        let k = j + AES_GCM_TAG_SIZE;
        let tag = x2[j..k].try_into().unwrap();
        if !noise.decrypt_and_hash_in_place(to_nonce(PACKET_TYPE_HANDSHAKE_RESPONSE, 0), &mut x2[i..j], tag) {
            return Err(byzantine_fault!(FailedAuth, true));
        }
        let ekem1_secret = e1_secret
            .decapsulate((&x2[i..j]).try_into().unwrap())
            .map(Secret)
            .ok_or(byzantine_fault!(FailedAuth, true))?;
        noise.mix_key(ekem1_secret.as_ref());
        drop(ekem1_secret);
        i = k;
        // We attempt to decrypt the payload at most three times. First two times with
        // the ratchet key Alice remembers, and final time with a ratchet
        // key of zero if Alice allows ratchet downgrades.
        // The following code is not constant time, meaning we leak to an
        // attacker whether or not we downgraded.
        // We don't currently consider this sensitive enough information to hide.
        let j = i + KID_SIZE;
        let k = j + AES_GCM_TAG_SIZE;
        let payload: [u8; KID_SIZE] = x2[i..j].try_into().unwrap();
        let tag = x2[j..k].try_into().unwrap();
        // Check for which ratchet key Bob wants to use.
        let test_ratchet_key = |ratchet_key| -> Option<(NonZeroU32, SymmetricState<App>)> {
            let mut noise = noise.clone();
            let mut payload = payload.clone();
            // Noise process pattern2 psk token.
            noise.mix_key_and_hash(ratchet_key);
            // Noise process pattern2 payload.
            if !noise.decrypt_and_hash_in_place(to_nonce(PACKET_TYPE_HANDSHAKE_RESPONSE, 0), &mut payload, tag) {
                return None;
            }
            NonZeroU32::new(u32::from_be_bytes(payload)).map(|kid2| (kid2, noise))
        };
        // Check first key.
        let mut ratchet_i = 0;
        let mut result = None;
        let mut chain_len = 0;
        if let Some(key) = zeta.ratchet_states[0].key() {
            chain_len = zeta.ratchet_states[0].chain_len();
            result = test_ratchet_key(key);
        }
        // Check second key.
        if result.is_none() {
            ratchet_i = 1;
            if let Some(key) = zeta.ratchet_states[1].key() {
                chain_len = zeta.ratchet_states[1].chain_len();
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

        let (kid_send, mut noise) = result.ok_or(byzantine_fault!(FailedAuth, true))?;
        let mut x3 = Vec::new();

        // Noise process pattern3 s token.
        let i = x3.len();
        x3.extend(&s_secret.public_key_bytes());
        noise.encrypt_and_hash_in_place(to_nonce(PACKET_TYPE_HANDSHAKE_COMPLETION, 1), i, &mut x3);
        // Noise process pattern3 se token.
        noise.mix_dh(&s_secret, &e_remote).ok_or(byzantine_fault!(FailedAuth, true))?;
        // Noise process pattern3 payload token.
        let i = x3.len();
        x3.extend(identity);
        noise.encrypt_and_hash_in_place(to_nonce(PACKET_TYPE_HANDSHAKE_COMPLETION, 0), i, &mut x3);

        let (rk, rf) = noise.get_ask(LABEL_RATCHET_STATE);
        let new_ratchet_state = RatchetState::new_incr(rk, rf, chain_len);

        let ratchet_to_preserve = &zeta.ratchet_states[ratchet_i];
        let result = app.save_ratchet_state(
            &zeta.s_remote,
            &zeta.application_data,
            [&zeta.ratchet_states[0], &zeta.ratchet_states[1]],
            [&new_ratchet_state, ratchet_to_preserve],
        );
        if let Err(e) = result {
            return Err(ReceiveError::RatchetIoError(e));
        }

        let (kek_recv, kek_send) = noise.get_ask(LABEL_KEX_KEY);
        let (nk_recv, nk_send) = noise.split();
        let n = to_nonce(PACKET_TYPE_HANDSHAKE_COMPLETION, 0);
        let identity = identity.clone();

        zeta.key_mut(true).send.kid = Some(kid_send);
        zeta.key_mut(true).send.kek = Some(kek_send);
        zeta.key_mut(true).send.nk = Some(nk_send);
        zeta.key_mut(true).recv.kek = Some(kek_recv);
        zeta.key_mut(true).recv.nk = Some(nk_recv);
        zeta.ratchet_states[1] = zeta.ratchet_states[ratchet_i].clone();
        zeta.ratchet_states[0] = new_ratchet_state;
        let current_time = app.time();
        zeta.key_creation_counter = zeta.send_counter;
        zeta.resend_timer = current_time + App::SETTINGS.resend_time;
        zeta.timeout_timer = current_time + App::SETTINGS.initial_offer_timeout;
        let packet = Packet(kid_send.get(), n, x3);
        zeta.beta = ZsspAutomata::A3 { identity, packet: packet.clone() };

        send(&packet, zeta.hk_send.as_ref());
        Ok(ReceiveOk::Unassociated)
    } else {
        Err(byzantine_fault!(FailedAuth, true))
    }
}
pub(crate) fn recv_x3_trans<App: ApplicationLayer>(
    zeta: StateB2<App>,
    app: &App,
    ctx: &Arc<ContextInner<App>>,
    kid: NonZeroU32,
    mut x3: Vec<u8>,
    send: impl FnOnce(&Packet, &[u8; AES_256_KEY_SIZE]),
) -> Result<Arc<Session<App>>, ReceiveError<App::IoError>> {
    use FaultType::*;

    if x3.len() < HANDSHAKE_COMPLETION_MIN_SIZE {
        return Err(byzantine_fault!(InvalidPacket, true));
    }
    if kid != zeta.kid_recv {
        return Err(byzantine_fault!(UnknownLocalKeyId, true));
    }

    let mut noise = zeta.noise.clone();
    let mut i = 0;
    // Noise process pattern3 s token.
    let j = i + P384_PUBLIC_KEY_SIZE;
    let k = j + AES_GCM_TAG_SIZE;
    let tag = x3[j..k].try_into().unwrap();
    if !noise.decrypt_and_hash_in_place(to_nonce(PACKET_TYPE_HANDSHAKE_COMPLETION, 1), &mut x3[i..j], tag) {
        return Err(byzantine_fault!(FailedAuth, true));
    }
    let s_remote = App::PublicKey::from_bytes((&x3[i..j]).try_into().unwrap()).ok_or(byzantine_fault!(FailedAuth, true))?;
    i = k;
    // Noise process pattern3 se token.
    noise.mix_dh(&zeta.e_secret, &s_remote).ok_or(byzantine_fault!(FailedAuth, true))?;
    // Noise process pattern3 payload.
    let k = x3.len();
    let j = k - AES_GCM_TAG_SIZE;
    let tag = x3[j..k].try_into().unwrap();
    if !noise.decrypt_and_hash_in_place(to_nonce(PACKET_TYPE_HANDSHAKE_COMPLETION, 0), &mut x3[i..j], tag) {
        return Err(byzantine_fault!(FailedAuth, true));
    }
    let identity_start = i;
    let identity_end = j;

    let (kek_send, kek_recv) = noise.get_ask(LABEL_KEX_KEY);
    let c = INIT_COUNTER;

    let (responder_disallows_downgrade, responder_silently_rejects) = app.check_accept_session(&s_remote, &x3[identity_start..identity_end]);
    let create_reject = || {
        let mut d = Vec::<u8>::new();
        let n = to_nonce(PACKET_TYPE_SESSION_REJECTED, c);
        let tag = App::Aead::encrypt_in_place(kek_send.as_ref(), n, None, &mut []);
        d.extend(&tag);
        // We just used a counter with this key, but we are not storing
        // the fact we used it in memory. This is currently ok because the
        // handshake is being dropped, so nonce reuse can't happen.
        Packet(zeta.kid_send.get(), n, d)
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
                        if !responder_silently_rejects {
                            send(&create_reject(), zeta.hk_send.as_ref())
                        }
                        return Err(byzantine_fault!(FailedAuth, true));
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
                    return Err(ReceiveError::RatchetIoError(e));
                }

                let mut c1 = Vec::new();
                let n = to_nonce(PACKET_TYPE_KEY_CONFIRM, c);
                let tag = App::Aead::encrypt_in_place(kek_send.as_ref(), n, None, &mut []);
                c1.extend(&tag);

                let (nk1, nk2) = noise.split();
                let keys = DuplexKey {
                    send: Keys { kek: Some(kek_send), nk: Some(nk1), kid: Some(zeta.kid_send) },
                    recv: Keys { kek: Some(kek_recv), nk: Some(nk2), kid: Some(zeta.kid_recv) },
                };
                let current_time = app.time();

                let mut session_map = ctx.session_map.lock().unwrap();
                use std::collections::hash_map::Entry::*;
                let entry = match session_map.entry(zeta.kid_recv) {
                    // We could have issued the kid that we initially offered Alice to someone else
                    // before Alice was able to respond. It is unlikely but possible.
                    Occupied(_) => return Err(byzantine_fault!(OutOfSequence, false)),
                    Vacant(entry) => entry,
                };
                let session = Arc::new(Session(Mutex::new(Zeta {
                    ctx: Arc::downgrade(ctx),
                    application_data,
                    was_bob: true,
                    s_remote,
                    send_counter: INIT_COUNTER + 1,
                    key_creation_counter: INIT_COUNTER + 1,
                    key_index: false,
                    keys: [keys, DuplexKey::default()],
                    ratchet_states: [new_ratchet_state, RatchetState::Null],
                    hk_send: zeta.hk_send.clone(),
                    resend_timer: current_time + App::SETTINGS.resend_time,
                    timeout_timer: current_time + App::SETTINGS.rekey_timeout,
                    beta: ZsspAutomata::S1,
                    counter_antireplay_window: std::array::from_fn(|_| 0),
                    defrag: zeta.defrag,
                })));
                entry.insert(Arc::downgrade(&session));
                ctx.sessions.lock().unwrap().insert(Arc::as_ptr(&session), Arc::downgrade(&session));

                send(&Packet(zeta.kid_send.get(), n, c1), zeta.hk_send.as_ref());
                Ok(session)
            }
            Err(e) => Err(ReceiveError::RatchetIoError(e)),
        }
    } else {
        if !responder_silently_rejects {
            send(&create_reject(), zeta.hk_send.as_ref())
        }
        Err(byzantine_fault!(FailedAuth, true))
    }
}
pub(crate) fn recv_c1_trans<App: ApplicationLayer>(
    zeta: &mut Zeta<App>,
    app: &App,
    rng: &Mutex<App::Rng>,
    kid: NonZeroU32,
    n: [u8; AES_GCM_IV_SIZE],
    c1: Vec<u8>,
    send: impl FnOnce(&Packet, &[u8; AES_256_KEY_SIZE]),
) -> Result<bool, ReceiveError<App::IoError>> {
    use FaultType::*;

    if c1.len() != KEY_CONFIRMATION_SIZE {
        return Err(byzantine_fault!(InvalidPacket, true));
    }
    let is_other = if Some(kid) == zeta.key_ref(true).recv.kid {
        true
    } else if Some(kid) == zeta.key_ref(false).recv.kid {
        false
    } else {
        // Some key confirmation may have arrived extremely delayed.
        // It is unlikely but possible.
        return Err(byzantine_fault!(OutOfSequence, false));
    };

    let specified_key = zeta.key_ref(is_other).recv.kek.as_ref().ok_or(byzantine_fault!(OutOfSequence, true))?;
    let tag = c1[..].try_into().unwrap();
    if !App::Aead::decrypt_in_place(specified_key.as_ref(), n, None, &mut [], tag) {
        return Err(byzantine_fault!(FailedAuth, true));
    }
    let (_, c) = from_nonce(&n);
    if !zeta.update_counter_window(c) {
        return Err(byzantine_fault!(ExpiredCounter, true));
    }

    let just_establised = is_other && matches!(&zeta.beta, ZsspAutomata::A3 { .. });
    if is_other {
        if let ZsspAutomata::A3 { .. } | ZsspAutomata::R2 { .. } = &zeta.beta {
            if !zeta.ratchet_states[1].is_null() {
                let result = app.save_ratchet_state(
                    &zeta.s_remote,
                    &zeta.application_data,
                    [&zeta.ratchet_states[0], &zeta.ratchet_states[1]],
                    [&zeta.ratchet_states[0], &RatchetState::Null],
                );
                if let Err(e) = result {
                    return Err(ReceiveError::RatchetIoError(e));
                }
            }

            zeta.ratchet_states[1] = RatchetState::Null;
            zeta.key_index ^= true;
            zeta.timeout_timer = app.time()
                + App::SETTINGS
                    .rekey_after_time
                    .saturating_sub(rng.lock().unwrap().next_u64() as i64 % App::SETTINGS.rekey_time_max_jitter);
            zeta.resend_timer = i64::MAX;
            zeta.beta = ZsspAutomata::S2;
        }
    }
    let mut c2 = Vec::new();

    let c = zeta.send_counter;
    zeta.send_counter += 1;
    let n = to_nonce(PACKET_TYPE_ACK, c);
    let latest_confirmed_key = zeta.key_ref(false).send.kek.as_ref().ok_or(byzantine_fault!(OutOfSequence, true))?;
    let tag = App::Aead::encrypt_in_place(latest_confirmed_key.as_ref(), n, None, &mut []);
    c2.extend(&tag);

    send(&Packet(zeta.key_ref(false).send.kid.unwrap().get(), n, c2), zeta.hk_send.as_ref());
    Ok(just_establised)
}
pub(crate) fn recv_c2_trans<App: ApplicationLayer>(
    zeta: &mut Zeta<App>,
    app: &App,
    rng: &Mutex<App::Rng>,
    kid: NonZeroU32,
    n: [u8; AES_GCM_IV_SIZE],
    c2: Vec<u8>,
) -> Result<(), ReceiveError<App::IoError>> {
    use FaultType::*;

    if c2.len() != ACKNOWLEDGEMENT_SIZE {
        return Err(byzantine_fault!(InvalidPacket, true));
    }
    if Some(kid) != zeta.key_ref(false).recv.kid {
        // Some acknowledgement may have arrived extremely delayed.
        return Err(byzantine_fault!(UnknownLocalKeyId, false));
    }
    if !matches!(&zeta.beta, ZsspAutomata::S1) {
        // Some acknowledgement may have arrived extremely delayed.
        return Err(byzantine_fault!(OutOfSequence, false));
    }

    let tag = c2[..].try_into().unwrap();
    if !App::Aead::decrypt_in_place(zeta.key_ref(false).recv.kek.as_ref().unwrap().as_ref(), n, None, &mut [], tag) {
        return Err(byzantine_fault!(FailedAuth, true));
    }
    let (_, c) = from_nonce(&n);
    if !zeta.update_counter_window(c) {
        return Err(byzantine_fault!(ExpiredCounter, true));
    }

    zeta.timeout_timer = app.time()
        + App::SETTINGS
            .rekey_after_time
            .saturating_sub(rng.lock().unwrap().next_u64() as i64 % App::SETTINGS.rekey_time_max_jitter);
    zeta.resend_timer = i64::MAX;
    zeta.beta = ZsspAutomata::S2;
    Ok(())
}
pub(crate) fn recv_d_trans<App: ApplicationLayer>(
    zeta: &mut Zeta<App>,
    kid: NonZeroU32,
    n: [u8; AES_GCM_IV_SIZE],
    d: Vec<u8>,
) -> Result<(), ReceiveError<App::IoError>> {
    use FaultType::*;

    if d.len() != SESSION_REJECTED_SIZE {
        return Err(byzantine_fault!(InvalidPacket, true));
    }
    if Some(kid) != zeta.key_ref(true).recv.kid || !matches!(&zeta.beta, ZsspAutomata::A3 { .. }) {
        return Err(byzantine_fault!(OutOfSequence, true));
    }

    let tag = d[..].try_into().unwrap();
    if !App::Aead::decrypt_in_place(zeta.key_ref(true).recv.kek.as_ref().unwrap().as_ref(), n, None, &mut [], tag) {
        return Err(byzantine_fault!(FailedAuth, true));
    }
    let (_, c) = from_nonce(&n);
    if !zeta.update_counter_window(c) {
        return Err(byzantine_fault!(ExpiredCounter, true));
    }

    zeta.expire();
    Ok(())
}
pub(crate) fn service<App: ApplicationLayer>(
    zeta: &mut Zeta<App>,
    session: &Arc<Session<App>>,
    ctx: &Arc<ContextInner<App>>,
    app: &App,
    current_time: i64,
    send: impl FnOnce(&Packet, Option<&[u8; AES_256_KEY_SIZE]>),
) {
    if zeta.timeout_timer <= current_time {
        timeout_trans(zeta, session, app, ctx, current_time, send);
    } else if zeta.resend_timer <= current_time {
        zeta.resend_timer = current_time + App::SETTINGS.resend_time;

        let (p, mut control_payload) = match &zeta.beta {
            ZsspAutomata::Null => return,
            ZsspAutomata::A1(StateA1 { packet, .. }) => {
                log!(app, ResentX1(session));
                return send(packet, None);
            }
            ZsspAutomata::A3 { packet, .. } => {
                log!(app, ResentX3(session));
                return send(packet, Some(zeta.hk_send.as_ref()));
            }
            ZsspAutomata::S1 => {
                log!(app, ResentKeyConfirm(session));
                (PACKET_TYPE_KEY_CONFIRM, Vec::new())
            }
            ZsspAutomata::S2 => return,
            ZsspAutomata::R1 { k1, .. } => {
                log!(app, ResentK1(session));
                (PACKET_TYPE_REKEY_INIT, k1.clone())
            }
            ZsspAutomata::R2 { k2, .. } => {
                log!(app, ResentK2(session));
                (PACKET_TYPE_REKEY_COMPLETE, k2.clone())
            }
        };
        let c = zeta.send_counter;
        zeta.send_counter += 1;
        let n = to_nonce(p, c);
        let tag = App::Aead::encrypt_in_place(zeta.key_ref(false).send.kek.as_ref().unwrap().as_ref(), n, None, &mut control_payload);
        control_payload.extend(&tag);
        send(
            &Packet(zeta.key_ref(false).send.kid.unwrap().get(), n, control_payload),
            Some(zeta.hk_send.as_ref()),
        );
    }
}
fn remap<App: ApplicationLayer>(session: &Arc<Session<App>>, zeta: &Zeta<App>, rng: &Mutex<App::Rng>, session_map: &SessionMap<App>) -> NonZeroU32 {
    let mut session_map = session_map.lock().unwrap();
    let weak = if let Some(Some(weak)) = zeta.key_ref(true).recv.kid.as_ref().map(|kid| session_map.remove(kid)) {
        weak
    } else {
        Arc::downgrade(&session)
    };
    let new_kid_recv = gen_kid(session_map.deref(), rng.lock().unwrap().deref_mut());
    session_map.insert(new_kid_recv, weak);
    new_kid_recv
}
#[allow(unused)]
fn timeout_trans<App: ApplicationLayer>(
    zeta: &mut Zeta<App>,
    session: &Arc<Session<App>>,
    app: &App,
    ctx: &Arc<ContextInner<App>>,
    current_time: i64,
    send: impl FnOnce(&Packet, Option<&[u8; AES_256_KEY_SIZE]>),
) {
    match &zeta.beta {
        ZsspAutomata::Null => {}
        ZsspAutomata::A1(StateA1 { identity, .. }) | ZsspAutomata::A3 { identity, .. } => {
            if matches!(&zeta.beta, ZsspAutomata::A1(_)) {
                log!(app, TimeoutX1(session));
            } else {
                log!(app, TimeoutX3(session));
            }
            let new_kid_recv = remap(session, &zeta, &ctx.rng, &ctx.session_map);

            if let Some(a1) = create_a1_state(&ctx.rng, &zeta.s_remote, new_kid_recv, &zeta.ratchet_states, identity.clone()) {
                let (hk_recv, hk_send) = a1.noise.get_ask(LABEL_HEADER_KEY);
                let packet = a1.packet.clone();

                zeta.hk_send = hk_send;
                *zeta.key_mut(true) = DuplexKey::default();
                zeta.key_mut(true).recv.kid = Some(new_kid_recv);
                zeta.resend_timer = current_time + App::SETTINGS.resend_time;
                zeta.timeout_timer = current_time + App::SETTINGS.initial_offer_timeout;
                zeta.beta = ZsspAutomata::A1(a1);
                zeta.defrag = DefragBuffer::new(Some(hk_recv));

                send(&packet, None);
            } else {
                zeta.expire();
            }
        }
        ZsspAutomata::S2 => {
            log!(app, StartedRekeyingSentK1(session));
            let new_kid_recv = remap(session, &zeta, &ctx.rng, &ctx.session_map);

            let mut k1 = Vec::new();
            let mut noise = SymmetricState::initialize(PROTOCOL_NAME_NOISE_KK);
            // Noise process prologue.
            noise.mix_hash(&ctx.s_secret.public_key_bytes());
            noise.mix_hash(&zeta.s_remote.to_bytes());
            // Noise process pattern1 psk0 token.
            noise.mix_key_and_hash(zeta.ratchet_states[0].key().unwrap());
            // Noise process pattern1 e token.
            let e_secret = noise.write_e(&ctx.rng, &mut k1);
            // Noise process pattern1 es token.
            if noise.mix_dh(&e_secret, &zeta.s_remote).is_none() {
                zeta.expire();
                return;
            }
            // Noise process pattern1 ss token.
            if noise.mix_dh(&ctx.s_secret, &zeta.s_remote).is_none() {
                zeta.expire();
                return;
            }
            // Noise process pattern1 payload token.
            let i = k1.len();
            k1.extend(&new_kid_recv.get().to_be_bytes());
            noise.encrypt_and_hash_in_place(to_nonce(PACKET_TYPE_REKEY_INIT, 0), i, &mut k1);

            zeta.key_mut(true).recv.kid = Some(new_kid_recv);
            zeta.timeout_timer = current_time + App::SETTINGS.rekey_timeout;
            zeta.resend_timer = current_time + App::SETTINGS.resend_time;
            zeta.beta = ZsspAutomata::R1 { noise, e_secret, k1: k1.clone() };

            let c = zeta.send_counter;
            zeta.send_counter += 1;
            let n = to_nonce(PACKET_TYPE_REKEY_INIT, c);
            let tag = App::Aead::encrypt_in_place(zeta.key_ref(false).send.kek.as_ref().unwrap().as_ref(), n, None, &mut k1);
            k1.extend(&tag);

            send(&Packet(zeta.key_ref(false).send.kid.unwrap().get(), n, k1), Some(zeta.hk_send.as_ref()));
        }
        ZsspAutomata::S1 { .. } => {
            log!(app, TimeoutKeyConfirm(session));
            zeta.expire();
        }
        ZsspAutomata::R1 { .. } => {
            log!(app, TimeoutK1(session));
            zeta.expire();
        }
        ZsspAutomata::R2 { .. } => {
            log!(app, TimeoutK2(session));
            zeta.expire();
        }
    }
}
pub(crate) fn recv_k1_trans<App: ApplicationLayer>(
    zeta: &mut Zeta<App>,
    session: &Arc<Session<App>>,
    app: &App,
    rng: &Mutex<App::Rng>,
    session_map: &SessionMap<App>,
    s_secret: &App::KeyPair,
    kid: NonZeroU32,
    n: [u8; AES_GCM_IV_SIZE],
    mut k1: Vec<u8>,
    send: impl FnOnce(&Packet, &[u8; AES_256_KEY_SIZE]),
) -> Result<(), ReceiveError<App::IoError>> {
    use FaultType::*;

    if k1.len() != REKEY_SIZE {
        return Err(byzantine_fault!(InvalidPacket, true));
    }
    if Some(kid) != zeta.key_ref(false).recv.kid {
        // Some rekey packet may have arrived extremely delayed.
        return Err(byzantine_fault!(UnknownLocalKeyId, false));
    }
    let should_rekey_as_bob = match &zeta.beta {
        ZsspAutomata::S2 { .. } => true,
        ZsspAutomata::R1 { .. } => zeta.was_bob,
        _ => false,
    };
    if !should_rekey_as_bob {
        // Some rekey packet may have arrived extremely delayed.
        return Err(byzantine_fault!(OutOfSequence, false));
    }

    let i = k1.len() - AES_GCM_TAG_SIZE;
    let tag = k1[i..].try_into().unwrap();
    if !App::Aead::decrypt_in_place(zeta.key_ref(false).recv.kek.as_ref().unwrap().as_ref(), n, None, &mut k1[..i], tag) {
        return Err(byzantine_fault!(FailedAuth, true));
    }
    let (_, c) = from_nonce(&n);
    if !zeta.update_counter_window(c) {
        return Err(byzantine_fault!(ExpiredCounter, true));
    }
    k1.truncate(i);

    let mut i = 0;
    let mut noise = SymmetricState::<App>::initialize(PROTOCOL_NAME_NOISE_KK);
    // Noise process prologue.
    noise.mix_hash(&zeta.s_remote.to_bytes());
    noise.mix_hash(&s_secret.public_key_bytes());
    // Noise process pattern1 psk0 token.
    noise.mix_key_and_hash(zeta.ratchet_states[0].key().unwrap());
    // Noise process pattern1 e token.
    let e_remote = noise.read_e(&mut i, &k1).ok_or(byzantine_fault!(FailedAuth, true))?;
    // Noise process pattern1 es token.
    noise.mix_dh(s_secret, &e_remote).ok_or(byzantine_fault!(FailedAuth, true))?;
    // Noise process pattern1 ss token.
    noise.mix_dh(s_secret, &zeta.s_remote).ok_or(byzantine_fault!(FailedAuth, true))?;
    // Noise process pattern1 payload.
    let j = i + KID_SIZE;
    let k = j + AES_GCM_TAG_SIZE;
    let tag = k1[j..k].try_into().unwrap();
    if !noise.decrypt_and_hash_in_place(to_nonce(PACKET_TYPE_REKEY_INIT, 0), &mut k1[i..j], tag) {
        return Err(byzantine_fault!(FailedAuth, true));
    }
    let kid_send = NonZeroU32::new(u32::from_be_bytes(k1[i..j].try_into().unwrap())).ok_or(byzantine_fault!(FailedAuth, true))?;

    let mut k2 = Vec::new();
    // Noise process pattern2 e token.
    let e_secret = noise.write_e(rng, &mut k2);
    // Noise process pattern2 ee token.
    noise.mix_dh(&e_secret, &e_remote).ok_or(byzantine_fault!(FailedAuth, true))?;
    // Noise process pattern2 se token.
    noise.mix_dh(&s_secret, &e_remote).ok_or(byzantine_fault!(FailedAuth, true))?;
    // Noise process pattern2 payload.
    let i = k2.len();
    let new_kid_recv = remap(session, &zeta, rng, session_map);
    k2.extend(&new_kid_recv.get().to_be_bytes());
    noise.encrypt_and_hash_in_place(to_nonce(PACKET_TYPE_REKEY_COMPLETE, 0), i, &mut k2);

    let (rk, rf) = noise.get_ask(LABEL_RATCHET_STATE);
    let new_ratchet_state = RatchetState::new_incr(rk, rf, zeta.ratchet_states[0].chain_len());
    let result = app.save_ratchet_state(
        &zeta.s_remote,
        &zeta.application_data,
        [&zeta.ratchet_states[0], &zeta.ratchet_states[1]],
        [&new_ratchet_state, &zeta.ratchet_states[0]],
    );
    if let Err(e) = result {
        return Err(ReceiveError::RatchetIoError(e));
    }
    let (kek_send, kek_recv) = noise.get_ask(LABEL_KEX_KEY);
    let (nk_send, nk_recv) = noise.split();

    zeta.key_mut(true).send.kid = Some(kid_send);
    zeta.key_mut(true).send.kek = Some(kek_send);
    zeta.key_mut(true).send.nk = Some(nk_send);
    zeta.key_mut(true).recv.kid = Some(new_kid_recv);
    zeta.key_mut(true).recv.kek = Some(kek_recv);
    zeta.key_mut(true).recv.nk = Some(nk_recv);
    zeta.ratchet_states[1] = zeta.ratchet_states[0].clone();
    zeta.ratchet_states[0] = new_ratchet_state;
    let current_time = app.time();
    zeta.key_creation_counter = zeta.send_counter;
    zeta.timeout_timer = current_time + App::SETTINGS.rekey_timeout;
    zeta.resend_timer = current_time + App::SETTINGS.resend_time;
    zeta.beta = ZsspAutomata::R2 { k2: k2.clone() };

    let c = zeta.send_counter;
    zeta.send_counter += 1;
    let n = to_nonce(PACKET_TYPE_REKEY_COMPLETE, c);
    let tag = App::Aead::encrypt_in_place(zeta.key_ref(false).send.kek.as_ref().unwrap().as_ref(), n, None, &mut k2);
    k2.extend(&tag);

    send(&Packet(zeta.key_ref(false).send.kid.unwrap().get(), n, k2), zeta.hk_send.as_ref());
    Ok(())
}
pub(crate) fn recv_k2_trans<App: ApplicationLayer>(
    zeta: &mut Zeta<App>,
    app: &App,
    kid: NonZeroU32,
    n: [u8; AES_GCM_IV_SIZE],
    mut k2: Vec<u8>,
    send: impl FnOnce(&Packet, &[u8; AES_256_KEY_SIZE]),
) -> Result<(), ReceiveError<App::IoError>> {
    use FaultType::*;

    if k2.len() != REKEY_SIZE {
        return Err(byzantine_fault!(InvalidPacket, true));
    }
    if Some(kid) != zeta.key_ref(false).recv.kid {
        // Some rekey packet may have arrived extremely delayed.
        return Err(byzantine_fault!(UnknownLocalKeyId, false));
    }
    if !matches!(&zeta.beta, ZsspAutomata::R1 { .. }) {
        // Some rekey packet may have arrived extremely delayed.
        return Err(byzantine_fault!(OutOfSequence, false));
    }

    let i = k2.len() - AES_GCM_TAG_SIZE;
    let tag = k2[i..].try_into().unwrap();
    if !App::Aead::decrypt_in_place(zeta.key_ref(false).recv.kek.as_ref().unwrap().as_ref(), n, None, &mut k2[..i], tag) {
        return Err(byzantine_fault!(FailedAuth, true));
    }
    let (_, c) = from_nonce(&n);
    if !zeta.update_counter_window(c) {
        return Err(byzantine_fault!(ExpiredCounter, true));
    }
    k2.truncate(i);

    if let ZsspAutomata::R1 { noise, e_secret, .. } = &zeta.beta {
        let mut noise = noise.clone();
        let mut i = 0;
        // Noise process pattern2 e token.
        let e_remote = noise.read_e(&mut i, &k2).ok_or(byzantine_fault!(FailedAuth, true))?;
        // Noise process pattern2 ee token.
        noise.mix_dh(e_secret, &e_remote).ok_or(byzantine_fault!(FailedAuth, true))?;
        // Noise process pattern2 se token.
        noise.mix_dh(e_secret, &zeta.s_remote).ok_or(byzantine_fault!(FailedAuth, true))?;
        // Noise process pattern2 payload.
        let j = i + KID_SIZE;
        let k = j + AES_GCM_TAG_SIZE;
        let tag = k2[j..k].try_into().unwrap();
        if !noise.decrypt_and_hash_in_place(to_nonce(PACKET_TYPE_REKEY_COMPLETE, 0), &mut k2[i..j], tag) {
            return Err(byzantine_fault!(FailedAuth, true));
        }
        let kid_send = NonZeroU32::new(u32::from_be_bytes(k2[i..j].try_into().unwrap())).ok_or(byzantine_fault!(InvalidPacket, true))?;

        let (rk, rf) = noise.get_ask(LABEL_RATCHET_STATE);
        let new_ratchet_state = RatchetState::new_incr(rk, rf, zeta.ratchet_states[0].chain_len());
        let result = app.save_ratchet_state(
            &zeta.s_remote,
            &zeta.application_data,
            [&zeta.ratchet_states[0], &zeta.ratchet_states[1]],
            [&new_ratchet_state, &RatchetState::Null],
        );
        if let Err(e) = result {
            return Err(ReceiveError::RatchetIoError(e));
        }
        let (kek_recv, kek_send) = noise.get_ask(LABEL_KEX_KEY);
        let (nk_recv, nk_send) = noise.split();

        zeta.key_mut(true).send.kid = Some(kid_send);
        zeta.key_mut(true).send.kek = Some(kek_send);
        zeta.key_mut(true).send.nk = Some(nk_send);
        zeta.key_mut(true).recv.kek = Some(kek_recv);
        zeta.key_mut(true).recv.nk = Some(nk_recv);
        zeta.ratchet_states[0] = new_ratchet_state;
        zeta.key_index ^= true;
        let current_time = app.time();
        zeta.key_creation_counter = zeta.send_counter;
        zeta.timeout_timer = current_time + App::SETTINGS.rekey_timeout;
        zeta.resend_timer = current_time + App::SETTINGS.resend_time;
        zeta.beta = ZsspAutomata::S1;

        let mut c1 = Vec::new();
        let c = zeta.send_counter;
        zeta.send_counter += 1;
        let n = to_nonce(PACKET_TYPE_KEY_CONFIRM, c);
        let tag = App::Aead::encrypt_in_place(zeta.key_ref(false).send.kek.as_ref().unwrap().as_ref(), n, None, &mut []);
        c1.extend(&tag);

        send(&Packet(zeta.key_ref(false).send.kid.unwrap().get(), n, c1), zeta.hk_send.as_ref());
        Ok(())
    } else {
        unreachable!()
    }
}
pub(crate) fn send_payload<App: ApplicationLayer>(
    zeta: &mut Zeta<App>,
    mut payload: Vec<u8>,
    send: impl FnOnce(&Packet, &[u8; AES_256_KEY_SIZE]),
) -> Result<(), SendError> {
    use SendError::*;

    if matches!(&zeta.beta, ZsspAutomata::Null) {
        return Err(SessionExpired);
    }
    if !matches!(
        &zeta.beta,
        ZsspAutomata::S1 | ZsspAutomata::S2 | ZsspAutomata::R1 { .. } | ZsspAutomata::R2 { .. }
    ) {
        return Err(SessionNotEstablished);
    }
    let c = zeta.send_counter;
    zeta.send_counter += 1;
    if c >= zeta.key_creation_counter + App::SETTINGS.rekey_after_key_uses {
        if c >= zeta.key_creation_counter + EXPIRE_AFTER_USES {
            zeta.expire();
        } else {
            // Cause timeout to occur next service interval.
            zeta.timeout_timer = i64::MIN;
        }
    }

    let n = to_nonce(PACKET_TYPE_DATA, c);
    let tag = App::Aead::encrypt_in_place(zeta.key_ref(false).send.nk.as_ref().unwrap().as_ref(), n, None, &mut payload);
    payload.extend(&tag);

    send(&Packet(zeta.key_ref(false).send.kid.unwrap().get(), n, payload), zeta.hk_send.as_ref());
    Ok(())
}
pub(crate) fn recv_payload_in_place<App: ApplicationLayer>(
    zeta: &mut Zeta<App>,
    kid: NonZeroU32,
    n: [u8; AES_GCM_IV_SIZE],
    payload: &mut Vec<u8>,
) -> Result<(), ReceiveError<App::IoError>> {
    use FaultType::*;

    if payload.len() < AES_GCM_TAG_SIZE {
        return Err(byzantine_fault!(FailedAuth, true));
    }
    let is_other = if Some(kid) == zeta.key_ref(true).recv.kid {
        true
    } else if Some(kid) == zeta.key_ref(false).recv.kid {
        false
    } else {
        // A packet would have to be delayed by around an hour for this error to occur, but it can
        // occur naturally due to just out-of-order transport.
        return Err(byzantine_fault!(OutOfSequence, false));
    };

    let i = payload.len() - AES_GCM_TAG_SIZE;
    let specified_key = zeta.key_ref(is_other).recv.nk.as_ref().ok_or(byzantine_fault!(OutOfSequence, true))?;
    let tag = payload[i..].try_into().unwrap();
    if !App::Aead::decrypt_in_place(specified_key.as_ref(), n, None, &mut payload[..i], tag) {
        return Err(byzantine_fault!(FailedAuth, true));
    }
    let (_, c) = from_nonce(&n);
    if !zeta.update_counter_window(c) {
        // This error is marked as not happening naturally, but it could occur if something about
        // the transport protocol is duplicating packets.
        return Err(byzantine_fault!(ExpiredCounter, true));
    }
    payload.truncate(i);

    Ok(())
}
impl<App: ApplicationLayer> Drop for Session<App> {
    fn drop(&mut self) {
        self.0.lock().unwrap().expire();
    }
}
