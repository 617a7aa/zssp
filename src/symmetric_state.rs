use std::marker::PhantomData;

use zeroize::Zeroizing;

use crate::applicationlayer::ApplicationLayer;
use crate::crypto::{AeadAesGcm, HashSha512, AES_256_KEY_SIZE, AES_GCM_IV_SIZE, AES_GCM_TAG_SIZE};
use crate::proto::{HASHLEN, LABEL_KBKDF_CHAIN};

pub struct SymmetricState<App: ApplicationLayer> {
    k: Zeroizing<[u8; AES_256_KEY_SIZE]>,
    ck: Zeroizing<[u8; HASHLEN]>,
    h: [u8; HASHLEN],
    /// If anyone knows a better way to get rid of the "parameter `App` is never used" error please
    /// let me know.
    _app: PhantomData<fn() -> App::SessionData>,
}
impl<App: ApplicationLayer> Clone for SymmetricState<App> {
    fn clone(&self) -> Self {
        Self {
            k: self.k.clone(),
            ck: self.ck.clone(),
            h: self.h.clone(),
            _app: PhantomData,
        }
    }
}

impl<App: ApplicationLayer> SymmetricState<App> {
    /// HMAC-SHA512 key derivation based on KBKDF Counter Mode:
    /// https://csrc.nist.gov/publications/detail/sp/800-108/rev-1/final.
    /// Cryptographically this isn't meaningfully different from
    /// `HKDF(self.chaining_key, input_key_material)` but this is how NIST rolls.
    /// These are the values we have assigned to the 4 variables involved in their KDF:
    /// * K_IN = `input_key_material`
    /// * Label = `label`
    /// * Context = `self.chaining_key`
    /// * L = `num_outputs*512u16`
    /// We have intentionally made every input small and fixed size to avoid unnecessary complexity
    /// and data representation ambiguity.
    /// Corresponds to Noise `HKDF`.
    fn kbkdf(
        &self,
        input_key_material: &[u8],
        label: &[u8; 4],
        num_outputs: u16,
        output1: &mut [u8; HASHLEN],
        output2: Option<&mut [u8; HASHLEN]>,
        output3: Option<&mut [u8; HASHLEN]>,
    ) {
        let mut buffer = Zeroizing::new(Vec::new());
        buffer.push(1);
        buffer.extend(label);
        buffer.push(0x00);
        buffer.extend(self.ck.as_ref());
        buffer.extend(&(num_outputs * 8 * HASHLEN as u16).to_be_bytes());

        debug_assert!(num_outputs >= 1);
        *output1 = App::Hash::hmac(input_key_material, &buffer);

        if let Some(output2) = output2 {
            debug_assert!(num_outputs >= 2);
            buffer[0] = 2;
            *output2 = App::Hash::hmac(input_key_material, &buffer);
        }

        if let Some(output3) = output3 {
            debug_assert!(num_outputs >= 3);
            buffer[0] = 3;
            *output3 = App::Hash::hmac(input_key_material, &buffer);
        }
    }

    /// Corresponds to Noise `Initialize` on a SymmetricState.
    pub fn initialize(h: &[u8; HASHLEN]) -> Self {
        Self {
            k: Zeroizing::default(),
            ck: Zeroizing::new(*h),
            h: *h,
            _app: PhantomData,
        }
    }
    /// Corresponds to Noise `MixKey`.
    pub fn mix_key(&mut self, input_key_material: &[u8]) {
        let mut next_ck = [0u8; HASHLEN];
        let mut temp_k = [0u8; HASHLEN];

        self.kbkdf(input_key_material, LABEL_KBKDF_CHAIN, 2, &mut next_ck, Some(&mut temp_k), None);

        *self.ck = next_ck;
        self.k.clone_from_slice(&temp_k[..AES_256_KEY_SIZE]);
    }
    /// Corresponds to Noise `MixHash`.
    pub fn mix_hash(&mut self, data: &[u8]) {
        let mut hash = App::Hash::new();
        hash.update(&self.h);
        hash.update(data);
        self.h = hash.finish();
    }
    /// Corresponds to Noise `MixKeyAndHash`.
    pub fn mix_key_and_hash(&mut self, input_key_material: &[u8]) {
        let mut next_ck = [0u8; HASHLEN];
        let mut temp_h = [0u8; HASHLEN];
        let mut temp_k = [0u8; HASHLEN];

        self.kbkdf(
            input_key_material,
            LABEL_KBKDF_CHAIN,
            3,
            &mut next_ck,
            Some(&mut temp_h),
            Some(&mut temp_k),
        );

        *self.ck = next_ck;
        self.mix_hash(&temp_h);
        self.k.clone_from_slice(&temp_k[..AES_256_KEY_SIZE]);
    }
    /// Corresponds to Noise `EncryptAndHash`.
    pub fn encrypt_and_hash_in_place(&mut self, iv: [u8; AES_GCM_IV_SIZE], plaintext_start: usize, buffer: &mut Vec<u8>) {
        let tag = App::Aead::encrypt_in_place(&self.k, iv, Some(&self.h), &mut buffer[plaintext_start..]);
        buffer.extend(&tag);
        self.mix_hash(&buffer[plaintext_start..]);
    }
    /// Corresponds to Noise `DecryptAndHash`.
    #[must_use]
    pub fn decrypt_and_hash_in_place(&mut self, iv: [u8; AES_GCM_IV_SIZE], buffer: &mut [u8], tag: [u8; AES_GCM_TAG_SIZE]) -> bool {
        let mut hash = App::Hash::new();
        hash.update(&self.h);
        hash.update(buffer);
        hash.update(&tag);
        let ret = App::Aead::decrypt_in_place(&self.k, iv, Some(&self.h), buffer, tag);
        self.h = hash.finish();
        ret
    }
    /// Corresponds to Noise `Split`.
    pub fn split(self) -> (Zeroizing<[u8; AES_256_KEY_SIZE]>, Zeroizing<[u8; AES_256_KEY_SIZE]>) {
        let mut temp_k1 = [0u8; HASHLEN];
        let mut temp_k2 = [0u8; HASHLEN];
        self.kbkdf(&[], LABEL_KBKDF_CHAIN, 2, &mut temp_k1, Some(&mut temp_k2), None);
        // Normally KBKDF would not truncate to derive the correct length of AES keys,
        // but Noise specifies that the AES keys be truncated from HASHLEN to AES_256_KEY_SIZE.
        (
            Zeroizing::new(temp_k1[..AES_256_KEY_SIZE].try_into().unwrap()),
            Zeroizing::new(temp_k2[..AES_256_KEY_SIZE].try_into().unwrap()),
        )
    }
    /// Get an additional symmetric key (ASK) that is a collision resistant hash of the transcript,
    /// is forward secrect and is cryptographically independent from all other produced keys.
    /// Based on Noise's unstable ASK mechanism, using KBKDF instead of HKDF.
    /// https://github.com/noiseprotocol/noise_wiki/wiki/Additional-Symmetric-Keys.
    pub fn get_ask(&self, label: &[u8; 4]) -> (Zeroizing<[u8; AES_256_KEY_SIZE]>, Zeroizing<[u8; AES_256_KEY_SIZE]>) {
        let mut temp_k1 = [0u8; HASHLEN];
        let mut temp_k2 = [0u8; HASHLEN];
        self.kbkdf(&self.h, label, 2, &mut temp_k1, Some(&mut temp_k2), None);
        (
            Zeroizing::new(temp_k1[..AES_256_KEY_SIZE].try_into().unwrap()),
            Zeroizing::new(temp_k2[..AES_256_KEY_SIZE].try_into().unwrap()),
        )
    }
    /// Used for internally debugging a key exchange.
    #[allow(unused)]
    pub(crate) fn finger(&self) -> (u8, u8, u8) {
        (self.k[0], self.ck[0], self.h[0])
    }
}
