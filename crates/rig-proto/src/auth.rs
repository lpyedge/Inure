//! 认证相关的纯函数：签名载荷、密钥编码、一次性凭证生成与哈希。
//!
//! 设计要点：
//! - Connector 身份 = 本地生成的 Ed25519 密钥对，私钥永不离开设备。
//! - 认证在应用层完成（challenge 签名），与 TLS 在哪一层终结无关。
//! - 一次性凭证（Grant token、stream_token、enrollment token）只以 SHA-256 哈希形式存库。

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};

use crate::ids::DeviceId;

/// 签名载荷的域分隔前缀。任何签名格式变化都必须换一个新前缀（v2、v3……）。
const AUTH_DOMAIN: &[u8] = b"rig-connector-auth-v1";

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("invalid base64")]
    Base64,
    #[error("invalid key or signature length")]
    Length,
    #[error("signature verification failed")]
    BadSignature,
}

/// 构造 challenge 签名载荷：域前缀、设备 ID、Gateway origin、nonce，以 0 字节分隔。
pub fn auth_payload(device_id: &DeviceId, gateway_origin: &str, nonce: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(
        AUTH_DOMAIN.len() + device_id.0.len() + gateway_origin.len() + nonce.len() + 3,
    );
    v.extend_from_slice(AUTH_DOMAIN);
    v.push(0);
    v.extend_from_slice(device_id.0.as_bytes());
    v.push(0);
    v.extend_from_slice(gateway_origin.as_bytes());
    v.push(0);
    v.extend_from_slice(nonce);
    v
}

pub fn generate_signing_key() -> SigningKey {
    SigningKey::generate(&mut OsRng)
}

pub fn sign_challenge(
    key: &SigningKey,
    device_id: &DeviceId,
    gateway_origin: &str,
    nonce: &[u8],
) -> String {
    let sig = key.sign(&auth_payload(device_id, gateway_origin, nonce));
    URL_SAFE_NO_PAD.encode(sig.to_bytes())
}

pub fn verify_challenge(
    public_key_b64: &str,
    device_id: &DeviceId,
    gateway_origin: &str,
    nonce: &[u8],
    signature_b64: &str,
) -> Result<(), AuthError> {
    let pk = decode_fixed::<32>(public_key_b64)?;
    let pk = VerifyingKey::from_bytes(&pk).map_err(|_| AuthError::Length)?;
    let sig = decode_fixed::<64>(signature_b64)?;
    let sig = Signature::from_bytes(&sig);
    pk.verify(&auth_payload(device_id, gateway_origin, nonce), &sig)
        .map_err(|_| AuthError::BadSignature)
}

pub fn encode_public_key(key: &SigningKey) -> String {
    URL_SAFE_NO_PAD.encode(key.verifying_key().to_bytes())
}

/// 生成 32 字节随机数，base64url 编码。用于 nonce、一次性凭证。
pub fn random_token() -> String {
    let mut b = [0u8; 32];
    OsRng.fill_bytes(&mut b);
    URL_SAFE_NO_PAD.encode(b)
}

/// 凭证入库前的哈希（十六进制 SHA-256）。数据库里永远不存凭证明文。
pub fn hash_token(token: &str) -> String {
    let d = Sha256::digest(token.as_bytes());
    d.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn decode_b64(s: &str) -> Result<Vec<u8>, AuthError> {
    URL_SAFE_NO_PAD.decode(s).map_err(|_| AuthError::Base64)
}

fn decode_fixed<const N: usize>(s: &str) -> Result<[u8; N], AuthError> {
    decode_b64(s)?.try_into().map_err(|_| AuthError::Length)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_and_verify_roundtrip() {
        let k = generate_signing_key();
        let dev = DeviceId("dev_a".into());
        let nonce = b"0123456789abcdef0123456789abcdef";
        let sig = sign_challenge(&k, &dev, "https://rig.example.com", nonce);
        verify_challenge(
            &encode_public_key(&k),
            &dev,
            "https://rig.example.com",
            nonce,
            &sig,
        )
        .unwrap();
    }

    #[test]
    fn signature_is_bound_to_origin_device_and_nonce() {
        let k = generate_signing_key();
        let pk = encode_public_key(&k);
        let dev = DeviceId("dev_a".into());
        let nonce = [7u8; 32];
        let sig = sign_challenge(&k, &dev, "https://a.example", &nonce);

        assert!(verify_challenge(&pk, &dev, "https://b.example", &nonce, &sig).is_err());
        assert!(verify_challenge(
            &pk,
            &DeviceId("dev_b".into()),
            "https://a.example",
            &nonce,
            &sig
        )
        .is_err());
        assert!(verify_challenge(&pk, &dev, "https://a.example", &[8u8; 32], &sig).is_err());
    }

    #[test]
    fn token_hash_is_stable_and_not_plaintext() {
        let t = random_token();
        assert_eq!(hash_token(&t), hash_token(&t));
        assert_ne!(hash_token(&t), t);
        assert_eq!(hash_token(&t).len(), 64);
    }
}
