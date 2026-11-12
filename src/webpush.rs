//! Web Push (RFC 8030): payload encryption (RFC 8291, aes128gcm) and VAPID (RFC 8292).
//! No push library: the whole protocol is a few HKDF steps and one AES-GCM record.

use aes_gcm::{Aes128Gcm, KeyInit, aead::Aead};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use hkdf::Hkdf;
use p256::{
    PublicKey, SecretKey,
    ecdsa::{Signature, SigningKey, signature::Signer},
    elliptic_curve::sec1::ToSec1Point,
};
use sha2::Sha256;

pub fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom").expect("urandom"), &mut b).expect("urandom");
    b
}

pub fn new_key() -> SecretKey {
    loop {
        if let Ok(k) = SecretKey::from_slice(&random::<32>()) {
            return k;
        }
    }
}

/// Uncompressed SEC1 point (65 bytes), the format browsers and WebCrypto "raw" import expect.
pub fn public_bytes(k: &SecretKey) -> Vec<u8> {
    k.public_key().to_sec1_point(false).as_bytes().to_vec()
}

pub fn b64(bytes: &[u8]) -> String {
    B64.encode(bytes)
}

pub fn unb64(s: &str) -> Result<Vec<u8>, String> {
    B64.decode(s.trim().trim_end_matches('=')).map_err(|_| "base64 inválido".to_string())
}

/// ECDSA P-256 / SHA-256 signature as raw r‖s, base64url (JWT ES256 and WebCrypto both use this form).
pub fn sign(key: &SecretKey, msg: &[u8]) -> String {
    let sig: Signature = SigningKey::from(key).sign(msg);
    b64(&sig.to_bytes())
}

/// One aes128gcm record for the subscription's keys (RFC 8291 §3–4).
pub fn encrypt(payload: &[u8], ua_public: &[u8], auth: &[u8], as_key: &SecretKey, salt: [u8; 16]) -> Result<Vec<u8>, String> {
    if payload.len() > 3800 {
        return Err("aviso demasiado largo".into());
    }
    let ua = PublicKey::from_sec1_bytes(ua_public).map_err(|_| "la suscripción trae una clave p256dh inválida")?;
    let shared = p256::ecdh::diffie_hellman(as_key.to_nonzero_scalar(), ua.as_affine());
    let as_public = public_bytes(as_key);

    let mut info = b"WebPush: info\0".to_vec();
    info.extend_from_slice(ua_public);
    info.extend_from_slice(&as_public);
    let mut ikm = [0u8; 32];
    Hkdf::<Sha256>::new(Some(auth), shared.raw_secret_bytes()).expand(&info, &mut ikm).map_err(|_| "hkdf")?;

    let prk = Hkdf::<Sha256>::new(Some(&salt), &ikm);
    let (mut cek, mut nonce) = ([0u8; 16], [0u8; 12]);
    prk.expand(b"Content-Encoding: aes128gcm\0", &mut cek).map_err(|_| "hkdf")?;
    prk.expand(b"Content-Encoding: nonce\0", &mut nonce).map_err(|_| "hkdf")?;

    let mut plain = payload.to_vec();
    plain.push(2); // padding delimiter of the last (only) record
    let sealed = Aes128Gcm::new(&cek.into()).encrypt(&nonce.into(), plain.as_slice()).map_err(|_| "aes-gcm")?;

    let mut out = salt.to_vec();
    out.extend_from_slice(&4096u32.to_be_bytes());
    out.push(as_public.len() as u8);
    out.extend_from_slice(&as_public);
    out.extend_from_slice(&sealed);
    Ok(out)
}

/// `Authorization` header for the push service of `endpoint` (RFC 8292).
pub fn vapid(endpoint: &str, key: &SecretKey, subject: &str) -> Result<String, String> {
    let aud = reqwest::Url::parse(endpoint).map_err(|_| "endpoint inválido")?.origin().ascii_serialization();
    let claims = serde_json::json!({ "aud": aud, "exp": crate::now() + 12 * 3600, "sub": subject });
    let input = format!("{}.{}", b64(br#"{"typ":"JWT","alg":"ES256"}"#), b64(claims.to_string().as_bytes()));
    let sig = sign(key, input.as_bytes());
    Ok(format!("vapid t={input}.{sig}, k={}", b64(&public_bytes(key))))
}

pub struct Sub {
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
    /// Where the app is served; the VAPID contact the push service sees.
    pub origin: String,
}

/// Sends one notification. Returns the push service status (404/410 mean the subscription is gone).
pub async fn send(http: &reqwest::Client, sub: &Sub, payload: &str, vapid_key: &SecretKey) -> Result<u16, String> {
    let body = encrypt(payload.as_bytes(), &unb64(&sub.p256dh)?, &unb64(&sub.auth)?, &new_key(), random())?;
    let r = http
        .post(&sub.endpoint)
        .timeout(std::time::Duration::from_secs(15))
        .header("authorization", vapid(&sub.endpoint, vapid_key, &sub.origin)?)
        .header("content-encoding", "aes128gcm")
        .header("content-type", "application/octet-stream")
        .header("ttl", "86400")
        .header("urgency", "high")
        .body(body)
        .send()
        .await
        .map_err(|e| format!("servicio de avisos: {e}"))?;
    Ok(r.status().as_u16())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 8291 Appendix A.
    #[test]
    fn rfc8291_example() {
        let as_key = SecretKey::from_slice(&unb64("yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw").unwrap()).unwrap();
        assert_eq!(
            b64(&public_bytes(&as_key)),
            "BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8"
        );
        let ua_public = unb64("BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4").unwrap();
        let auth = unb64("BTBZMqHH6r4Tts7J_aSIgg").unwrap();
        let salt: [u8; 16] = unb64("DGv6ra1nlYgDCS1FRnbzlw").unwrap().try_into().unwrap();
        let out = encrypt(b"When I grow up, I want to be a watermelon", &ua_public, &auth, &as_key, salt).unwrap();
        // The RFC's example uses a 4096-byte record size, like this implementation.
        assert_eq!(
            b64(&out),
            "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPTpK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN"
        );
    }

    #[test]
    fn vapid_header_shape() {
        let k = new_key();
        let h = vapid("https://fcm.googleapis.com/fcm/send/abc", &k, "https://vigia.example").unwrap();
        let (t, key) = h.strip_prefix("vapid t=").unwrap().split_once(", k=").unwrap();
        assert_eq!(t.split('.').count(), 3);
        assert_eq!(unb64(key).unwrap().len(), 65);
        let claims: serde_json::Value = serde_json::from_slice(&unb64(t.split('.').nth(1).unwrap()).unwrap()).unwrap();
        assert_eq!(claims["aud"], "https://fcm.googleapis.com");
    }
}
