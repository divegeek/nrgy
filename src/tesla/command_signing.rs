//! Tesla vehicle command signing: AES-GCM (BLE) and HMAC (cloud proxy) schemes.
//!
//! Protocol reference: https://github.com/teslamotors/vehicle-command
//!
//! Key derivation: ECDH on P-256 → SHA-1(x_coordinate)[0..16] → AES-128 session key.
//! Metadata TLV:   tag(u8) | len(u8) | value... per field, terminated with TAG_END (0xFF),
//!                 fed into SHA-256 (→ AES-GCM AAD) or HMAC-SHA-256 subkey (→ HMAC tag).

use std::time::{Duration, SystemTime};

use aes_gcm::{Aes128Gcm, KeyInit, aead::Aead};
use hmac::{Hmac, Mac};
use p256::pkcs8::DecodePrivateKey;
use p256::{SecretKey, ecdh::diffie_hellman, elliptic_curve::sec1::ToEncodedPoint};
use sha1::Digest as Sha1Digest;
use sha2::Sha256;
use thiserror::Error;

use super::proto::signatures::{
    AesGcmPersonalizedSignatureData, HmacPersonalizedSignatureData, KeyIdentity, SessionInfo,
    SignatureData, SignatureType, Tag, key_identity::IdentityType, signature_data::SigType,
};
use super::proto::universal_message::{
    Destination, Domain, RoutableMessage, SessionInfoRequest,
    destination::SubDestination,
    routable_message::{Payload, SubSigData},
};

const LABEL_MESSAGE_AUTH: &str = "authenticated command";
const EPOCH_LEN: usize = 16;
const SESSION_KEY_LEN: usize = 16;

#[derive(Error, Debug)]
pub enum SigningError {
    #[error("Invalid PEM key: {0}")]
    InvalidKey(String),
    #[error("Invalid vehicle public key")]
    InvalidVehicleKey,
    #[error("Session not initialized — call update_session first")]
    NoSession,
    #[error("Counter rollover; epoch must be rotated")]
    CounterRollover,
    #[error("Metadata field too long (max 255 bytes)")]
    MetadataFieldTooLong,
    #[error("Metadata tags must be added in increasing order")]
    MetadataOutOfOrder,
    #[error("Message has no ProtobufMessageAsBytes payload")]
    NoPayload,
    #[error("Message has no domain destination")]
    NoDomain,
    #[error("Expiration time out of range")]
    BadExpiration,
    #[error("Encryption failed")]
    EncryptionError,
}

pub type SigningResult<T> = Result<T, SigningError>;

// ── Key loading ───────────────────────────────────────────────────────────────

/// Load a P-256 private key from a PKCS8 PEM string (`BEGIN PRIVATE KEY`).
pub fn load_private_key(pem: &str) -> SigningResult<SecretKey> {
    SecretKey::from_pkcs8_pem(pem).map_err(|e| SigningError::InvalidKey(e.to_string()))
}

// ── Session key derivation ────────────────────────────────────────────────────

/// Derive the 16-byte AES session key from a P-256 ECDH exchange.
///
/// Tesla KDF: `SHA-1(x_coordinate)[0..16]` where `x_coordinate` is the
/// 32-byte big-endian zero-padded x-coord of the ECDH shared point.
pub fn derive_session_key(
    private_key: &SecretKey,
    vehicle_public_bytes: &[u8],
) -> SigningResult<[u8; SESSION_KEY_LEN]> {
    let vehicle_public = p256::PublicKey::from_sec1_bytes(vehicle_public_bytes)
        .map_err(|_| SigningError::InvalidVehicleKey)?;

    let shared = diffie_hellman(private_key.to_nonzero_scalar(), vehicle_public.as_affine());
    let digest = sha1::Sha1::digest(shared.raw_secret_bytes().as_slice());
    Ok(digest[..SESSION_KEY_LEN].try_into().unwrap())
}

// ── Metadata TLV ─────────────────────────────────────────────────────────────

struct MetadataTlv {
    buf: Vec<u8>,
    last_tag: u8,
}

impl MetadataTlv {
    fn new() -> Self {
        Self {
            buf: Vec::new(),
            last_tag: 0,
        }
    }

    fn add(&mut self, tag: u8, value: &[u8]) -> SigningResult<()> {
        if tag < self.last_tag {
            return Err(SigningError::MetadataOutOfOrder);
        }
        if value.len() > 255 {
            return Err(SigningError::MetadataFieldTooLong);
        }
        self.last_tag = tag;
        self.buf.push(tag);
        self.buf.push(value.len() as u8);
        self.buf.extend_from_slice(value);
        Ok(())
    }

    fn add_u32(&mut self, tag: u8, value: u32) -> SigningResult<()> {
        self.add(tag, &value.to_be_bytes())
    }

    /// SHA-256(fields || 0xFF || extra).  Used as AES-GCM AAD (extra = empty).
    fn sha256_checksum(mut self, extra: &[u8]) -> Vec<u8> {
        self.buf.push(Tag::End as u8);
        self.buf.extend_from_slice(extra);
        Sha256::digest(&self.buf).to_vec()
    }

    /// HMAC-SHA-256(subkey, fields || 0xFF || plaintext).  Used for HMAC signing.
    fn hmac_checksum(mut self, subkey: &[u8], plaintext: &[u8]) -> Vec<u8> {
        self.buf.push(Tag::End as u8);
        self.buf.extend_from_slice(plaintext);
        // Use local `use` to avoid colliding with aes_gcm::KeyInit at the module level.
        use hmac::digest::KeyInit as HmacKeyInit;
        let mut mac = Hmac::<Sha256>::new_from_slice(subkey).expect("HMAC accepts any key size");
        mac.update(&self.buf);
        mac.finalize().into_bytes().to_vec()
    }
}

// ── Subkey derivation ─────────────────────────────────────────────────────────

fn derive_subkey(session_key: &[u8], label: &str) -> Vec<u8> {
    use hmac::digest::KeyInit as HmacKeyInit;
    let mut mac = Hmac::<Sha256>::new_from_slice(session_key).expect("HMAC accepts any key size");
    mac.update(label.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

// ── CommandSigner ─────────────────────────────────────────────────────────────

/// Signs and encrypts Tesla vehicle commands.
///
/// Call [`CommandSigner::update_session`] with the `SessionInfo` received from
/// the vehicle handshake before calling [`encrypt`] or [`authorize_hmac`].
pub struct CommandSigner {
    private_key: SecretKey,
    vin: Vec<u8>,
    session_key: Option<[u8; SESSION_KEY_LEN]>,
    local_public: Vec<u8>,
    epoch: [u8; EPOCH_LEN],
    counter: u32,
    time_zero: SystemTime,
}

#[expect(dead_code)]
impl CommandSigner {
    pub fn new(pem: &str, vin: &str) -> SigningResult<Self> {
        let private_key = load_private_key(pem)?;
        let local_public = private_key
            .public_key()
            .to_encoded_point(false)
            .as_bytes()
            .to_vec();
        Ok(Self {
            private_key,
            vin: vin.as_bytes().to_vec(),
            session_key: None,
            local_public,
            epoch: [0u8; EPOCH_LEN],
            counter: 0,
            time_zero: SystemTime::UNIX_EPOCH,
        })
    }

    pub fn public_key_bytes(&self) -> &[u8] {
        &self.local_public
    }

    pub fn has_session(&self) -> bool {
        self.session_key.is_some()
    }

    pub fn invalidate_session(&mut self) {
        self.session_key = None;
    }

    /// Build a `RoutableMessage` containing a `SessionInfoRequest` for the given domain.
    /// Send the serialized form to the vehicle; it responds with a `SessionInfo`.
    pub fn session_info_request(&self, domain: Domain) -> RoutableMessage {
        let uuid: [u8; 16] = rand::random();
        let routing_addr: [u8; 16] = rand::random();
        RoutableMessage {
            to_destination: Some(Destination {
                sub_destination: Some(SubDestination::Domain(domain as i32)),
            }),
            from_destination: Some(Destination {
                sub_destination: Some(SubDestination::RoutingAddress(routing_addr.to_vec())),
            }),
            payload: Some(Payload::SessionInfoRequest(SessionInfoRequest {
                public_key: self.local_public.clone(),
                challenge: rand::random::<[u8; 8]>().to_vec(),
            })),
            uuid: uuid.to_vec(),
            ..Default::default()
        }
    }

    /// Update session state from the `SessionInfo` returned by the vehicle handshake.
    pub fn update_session(&mut self, info: &SessionInfo) -> SigningResult<()> {
        self.session_key = Some(derive_session_key(&self.private_key, &info.public_key)?);
        self.counter = info.counter;
        let copy_len = info.epoch.len().min(EPOCH_LEN);
        self.epoch[..copy_len].copy_from_slice(&info.epoch[..copy_len]);
        self.time_zero = SystemTime::now()
            .checked_sub(Duration::from_secs(info.clock_time as u64))
            .unwrap_or(SystemTime::UNIX_EPOCH);
        Ok(())
    }

    fn session_key(&self) -> SigningResult<[u8; SESSION_KEY_LEN]> {
        self.session_key.ok_or(SigningError::NoSession)
    }

    fn next_counter(&mut self) -> SigningResult<u32> {
        if self.counter == u32::MAX {
            return Err(SigningError::CounterRollover);
        }
        self.counter += 1;
        Ok(self.counter)
    }

    fn expires_at(&self, ttl: Duration) -> SigningResult<u32> {
        let elapsed = self.time_zero.elapsed().unwrap_or(Duration::ZERO);
        let t = (elapsed + ttl).as_secs();
        if t > u32::MAX as u64 {
            return Err(SigningError::BadExpiration);
        }
        Ok(t as u32)
    }

    fn domain_byte(message: &RoutableMessage) -> SigningResult<u8> {
        match message
            .to_destination
            .as_ref()
            .and_then(|d| d.sub_destination.as_ref())
        {
            Some(SubDestination::Domain(d)) => Ok(*d as u8),
            _ => Err(SigningError::NoDomain),
        }
    }

    fn build_metadata(
        &self,
        sig_type: SignatureType,
        domain: u8,
        expires_at: u32,
        counter: u32,
        flags: u32,
    ) -> SigningResult<MetadataTlv> {
        let mut meta = MetadataTlv::new();
        meta.add(Tag::SignatureType as u8, &[sig_type as u8])?;
        meta.add(Tag::Domain as u8, &[domain])?;
        meta.add(Tag::Personalization as u8, &self.vin)?;
        meta.add(Tag::Epoch as u8, &self.epoch)?;
        meta.add_u32(Tag::ExpiresAt as u8, expires_at)?;
        meta.add_u32(Tag::Counter as u8, counter)?;
        if flags != 0 {
            meta.add_u32(Tag::Flags as u8, flags)?;
        }
        Ok(meta)
    }

    /// Encrypt the message payload with AES-128-GCM (for BLE transport).
    ///
    /// The `payload` field must be `ProtobufMessageAsBytes`.  On return it holds
    /// the ciphertext and `sub_sig_data` holds the GCM nonce, epoch, counter, and tag.
    pub fn encrypt(&mut self, message: &mut RoutableMessage, ttl: Duration) -> SigningResult<()> {
        let session_key = self.session_key()?;
        let counter = self.next_counter()?;
        let expires_at = self.expires_at(ttl)?;
        let domain = Self::domain_byte(message)?;

        let plaintext = match message.payload.take() {
            Some(Payload::ProtobufMessageAsBytes(b)) => b,
            _ => return Err(SigningError::NoPayload),
        };

        let meta = self.build_metadata(
            SignatureType::AesGcmPersonalized,
            domain,
            expires_at,
            counter,
            message.flags,
        )?;
        let aad = meta.sha256_checksum(&[]);

        let nonce_bytes: [u8; 12] = rand::random();
        let nonce = aes_gcm::Nonce::from_slice(&nonce_bytes);
        let cipher =
            Aes128Gcm::new_from_slice(&session_key).map_err(|_| SigningError::EncryptionError)?;
        let ct_and_tag = cipher
            .encrypt(
                nonce,
                aes_gcm::aead::Payload {
                    msg: &plaintext,
                    aad: &aad,
                },
            )
            .map_err(|_| SigningError::EncryptionError)?;

        let ciphertext = ct_and_tag[..plaintext.len()].to_vec();
        let gcm_tag = ct_and_tag[plaintext.len()..].to_vec();

        message.payload = Some(Payload::ProtobufMessageAsBytes(ciphertext));
        message.sub_sig_data = Some(SubSigData::SignatureData(SignatureData {
            signer_identity: Some(KeyIdentity {
                identity_type: Some(IdentityType::PublicKey(self.local_public.clone())),
            }),
            sig_type: Some(SigType::AesGcmPersonalizedData(
                AesGcmPersonalizedSignatureData {
                    epoch: self.epoch.to_vec(),
                    nonce: nonce_bytes.to_vec(),
                    counter,
                    expires_at,
                    tag: gcm_tag,
                },
            )),
        }));
        Ok(())
    }

    /// Add HMAC authentication tag to message (for HTTP proxy / cloud transport).
    ///
    /// The payload is NOT encrypted; the HMAC authenticates both metadata and payload.
    /// Use this when sending via the local tesla-http-proxy, which needs to inspect commands.
    pub fn authorize_hmac(
        &mut self,
        message: &mut RoutableMessage,
        ttl: Duration,
    ) -> SigningResult<()> {
        let session_key = self.session_key()?;
        let counter = self.next_counter()?;
        let expires_at = self.expires_at(ttl)?;
        let domain = Self::domain_byte(message)?;

        let plaintext = match &message.payload {
            Some(Payload::ProtobufMessageAsBytes(b)) => b.clone(),
            _ => return Err(SigningError::NoPayload),
        };

        let meta = self.build_metadata(
            SignatureType::HmacPersonalized,
            domain,
            expires_at,
            counter,
            message.flags,
        )?;
        let subkey = derive_subkey(&session_key, LABEL_MESSAGE_AUTH);
        let tag = meta.hmac_checksum(&subkey, &plaintext);

        message.sub_sig_data = Some(SubSigData::SignatureData(SignatureData {
            signer_identity: Some(KeyIdentity {
                identity_type: Some(IdentityType::PublicKey(self.local_public.clone())),
            }),
            sig_type: Some(SigType::HmacPersonalizedData(
                HmacPersonalizedSignatureData {
                    epoch: self.epoch.to_vec(),
                    counter,
                    expires_at,
                    tag,
                },
            )),
        }));
        Ok(())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Test vectors from teslamotors/vehicle-command internal/authentication/ecdh_test.go
    // The private key / public key pair is chosen so the shared secret has a leading 0x00
    // byte, exercising zero-padding in the x-coordinate.
    #[test]
    fn test_ecdh_session_key() {
        let private_scalar = [
            0x52, 0x60, 0xf8, 0xd6, 0x11, 0x38, 0x75, 0xd8, 0x6f, 0x8e, 0xe8, 0xfe, 0xa3, 0x40,
            0xdf, 0x1f, 0xfb, 0x40, 0xc6, 0x58, 0xb5, 0x45, 0x5e, 0x8c, 0x33, 0xd7, 0x97, 0xc5,
            0x3a, 0x41, 0xaf, 0xd3,
        ];
        let phone_public = [
            0x04, 0x07, 0xfb, 0x60, 0xb6, 0x5b, 0x94, 0xe0, 0xde, 0x4a, 0x95, 0x4c, 0x53, 0xbe,
            0x10, 0x00, 0x3d, 0x9e, 0x69, 0x91, 0x8d, 0xed, 0xfd, 0xa5, 0xf4, 0xe9, 0xef, 0xb9,
            0xeb, 0xd8, 0xc5, 0xbd, 0x67, 0x2a, 0x53, 0x99, 0x1c, 0x40, 0x68, 0x86, 0x5d, 0x5f,
            0xb4, 0x4f, 0x97, 0xf6, 0xce, 0xcf, 0x83, 0x98, 0xf2, 0x61, 0xdd, 0x1d, 0x7b, 0xc6,
            0x9b, 0xe6, 0x76, 0xaf, 0xdc, 0x8f, 0xfa, 0xcb, 0xcc,
        ];
        // Expected ECDH x-coordinate with leading 0x00 (tests zero-padding)
        let correct_x = [
            0x00u8, 0x72, 0xd5, 0xb8, 0x15, 0x20, 0x7a, 0x04, 0xf0, 0xc7, 0x95, 0xfb, 0xa0, 0xba,
            0x9e, 0x8a, 0xdd, 0x3f, 0x1f, 0x57, 0x14, 0x8c, 0x51, 0xff, 0xac, 0xe2, 0x2c, 0xa1,
            0x5e, 0x6f, 0xd8, 0x45,
        ];
        let expected: [u8; SESSION_KEY_LEN] = sha1::Sha1::digest(correct_x)[..SESSION_KEY_LEN]
            .try_into()
            .unwrap();

        let sk = SecretKey::from_slice(&private_scalar).expect("valid test key");
        let got = derive_session_key(&sk, &phone_public).expect("ECDH should succeed");
        assert_eq!(got, expected);
    }

    // Test vectors from teslamotors/vehicle-command internal/authentication/metadata_test.go
    #[test]
    fn test_metadata_checksum() {
        let epoch = [
            0xaa, 0xda, 0x92, 0x8a, 0x4f, 0x21, 0x5f, 0x55, 0xf9, 0xe6, 0xe4, 0x5e, 0x66, 0xb6,
            0x52, 0x1e,
        ];
        let expected = [
            0xab, 0xab, 0x04, 0xd8, 0x04, 0x49, 0x98, 0x13, 0x38, 0x2e, 0xfd, 0x74, 0xa0, 0x67,
            0x91, 0xce, 0x2d, 0xe7, 0x77, 0x43, 0x96, 0x03, 0x24, 0x6d, 0xfb, 0xaa, 0x83, 0x92,
            0xca, 0x05, 0x86, 0x8e,
        ];

        let mut meta = MetadataTlv::new();
        meta.add(
            Tag::SignatureType as u8,
            &[SignatureType::AesGcmPersonalized as u8],
        )
        .unwrap();
        meta.add(Tag::Domain as u8, &[0x02]).unwrap();
        meta.add(Tag::Personalization as u8, b"testVIN").unwrap();
        meta.add(Tag::Epoch as u8, &epoch).unwrap();
        meta.add_u32(Tag::ExpiresAt as u8, 0x0000_0e74).unwrap();
        meta.add_u32(Tag::Counter as u8, 0x0000_053a).unwrap();

        assert_eq!(meta.sha256_checksum(&[]).as_slice(), expected);
    }

    #[test]
    fn test_metadata_out_of_order() {
        let mut meta = MetadataTlv::new();
        meta.add(Tag::Domain as u8, &[0x02]).unwrap();
        assert!(matches!(
            meta.add(Tag::SignatureType as u8, &[0x05]),
            Err(SigningError::MetadataOutOfOrder)
        ));
    }

    #[test]
    fn test_metadata_field_too_long() {
        let mut meta = MetadataTlv::new();
        assert!(matches!(
            meta.add(Tag::Personalization as u8, &[0u8; 256]),
            Err(SigningError::MetadataFieldTooLong)
        ));
    }
}
