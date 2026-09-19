//! Minecraft 26.2 signed-chat payload construction and RSA verification.
//!
//! The byte layout here follows the 26.2 server bytecode for
//! `PlayerChatMessage.updateSignature`, `SignedMessageLink.updateSignature`,
//! `SignedMessageBody.updateSignature`, and `LastSeenMessages.updateSignature`.
//! This module deliberately does not own chain state or decide whether a
//! packet should be accepted by a server.

use rsa::RsaPublicKey;
use rsa::pkcs1v15::{Signature as RsaSignature, VerifyingKey};
use rsa::pkcs8::DecodePublicKey;
use rsa::signature::Verifier;
use sha2::Sha256;
use thiserror::Error;

use super::{SignedMessageBody, SignedMessageLink};

/// The RSA signature size used by Minecraft chat session keys.
pub const CHAT_SIGNATURE_LEN: usize = 256;

/// Errors produced while constructing or checking a signed-chat message.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ChatSignatureError {
    #[error("last-seen signature at index {index} has length {length}, expected {expected}")]
    InvalidLastSeenSignatureLength {
        index: usize,
        length: usize,
        expected: usize,
    },
    #[error("chat content is too long to encode as a signed-message int32 length")]
    ContentTooLong,
    #[error("too many last-seen signatures to encode as a signed-message int32 count")]
    LastSeenTooLong,
    #[error("chat public key is not a valid DER SubjectPublicKeyInfo")]
    InvalidPublicKey,
    #[error("chat signature has length {length}, expected {expected}")]
    InvalidSignatureLength { length: usize, expected: usize },
    #[error("chat signature does not match the canonical payload")]
    SignatureMismatch,
}

/// Builds the exact bytes signed by a 26.2 Minecraft client.
///
/// `SignedMessageBody::time_stamp` is the serverbound wire timestamp in
/// epoch milliseconds. The signed body uses `Instant.getEpochSecond()`, so
/// conversion is floor division rather than writing milliseconds directly.
/// UUIDs are written in their 16-byte RFC 4122 representation.
pub fn canonical_bytes(
    link: &SignedMessageLink,
    body: &SignedMessageBody,
) -> Result<Vec<u8>, ChatSignatureError> {
    if body.content.len() > i32::MAX as usize {
        return Err(ChatSignatureError::ContentTooLong);
    }
    if body.last_seen.len() > i32::MAX as usize {
        return Err(ChatSignatureError::LastSeenTooLong);
    }
    for (index, signature) in body.last_seen.iter().enumerate() {
        if signature.len() != CHAT_SIGNATURE_LEN {
            return Err(ChatSignatureError::InvalidLastSeenSignatureLength {
                index,
                length: signature.len(),
                expected: CHAT_SIGNATURE_LEN,
            });
        }
    }

    let mut bytes = Vec::with_capacity(
        4 + 16
            + 16
            + 4
            + 8
            + 8
            + 4
            + body.content.len()
            + 4
            + body.last_seen.len() * CHAT_SIGNATURE_LEN,
    );
    bytes.extend_from_slice(&1_i32.to_be_bytes());
    bytes.extend_from_slice(link.sender.as_bytes());
    bytes.extend_from_slice(link.session_id.as_bytes());
    bytes.extend_from_slice(&link.index.to_be_bytes());
    bytes.extend_from_slice(&body.salt.to_be_bytes());
    bytes.extend_from_slice(&body.time_stamp.div_euclid(1_000).to_be_bytes());
    bytes.extend_from_slice(&(body.content.len() as i32).to_be_bytes());
    bytes.extend_from_slice(body.content.as_bytes());
    bytes.extend_from_slice(&(body.last_seen.len() as i32).to_be_bytes());
    for signature in &body.last_seen {
        bytes.extend_from_slice(signature);
    }
    Ok(bytes)
}

/// Verifies an RSA PKCS#1 v1.5/SHA-256 signature over already-canonical bytes.
///
/// `public_key_der` is the session public key in DER SubjectPublicKeyInfo
/// form. This is intentionally separate from Mojang's SHA-1 session-certificate
/// verification.
pub fn verify_canonical_signature(
    public_key_der: &[u8],
    payload: &[u8],
    signature: &[u8],
) -> Result<(), ChatSignatureError> {
    if signature.len() != CHAT_SIGNATURE_LEN {
        return Err(ChatSignatureError::InvalidSignatureLength {
            length: signature.len(),
            expected: CHAT_SIGNATURE_LEN,
        });
    }

    let public_key = RsaPublicKey::from_public_key_der(public_key_der)
        .map_err(|_| ChatSignatureError::InvalidPublicKey)?;
    let signature = RsaSignature::try_from(signature).map_err(|_| {
        ChatSignatureError::InvalidSignatureLength {
            length: signature.len(),
            expected: CHAT_SIGNATURE_LEN,
        }
    })?;
    VerifyingKey::<Sha256>::new(public_key)
        .verify(payload, &signature)
        .map_err(|_| ChatSignatureError::SignatureMismatch)
}

/// Builds and verifies one signed chat message without changing chain state.
pub fn verify_chat_message_signature(
    public_key_der: &[u8],
    link: &SignedMessageLink,
    body: &SignedMessageBody,
    signature: &[u8],
) -> Result<(), ChatSignatureError> {
    let payload = canonical_bytes(link, body)?;
    verify_canonical_signature(public_key_der, &payload, signature)
}

#[cfg(test)]
mod tests {
    use super::{
        CHAT_SIGNATURE_LEN, ChatSignatureError, canonical_bytes, verify_chat_message_signature,
    };
    use crate::net::chat::{SignedMessageBody, SignedMessageLink};
    use serde_json::Value;
    use sha2::{Digest, Sha256};
    use uuid::Uuid;

    const INDEPENDENT_FIXTURE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tools/compat-26_2/chat-fixtures/signed-command-independent.json"
    ));

    const CANONICAL_HEX: &str = "000000010000000000000000000000000000000100000000000000000000000000000002000000000102030405060708000000006553f10000000002686900000000";
    const CANONICAL_SHA256_HEX: &str =
        "6ca6cc185f024bc125cd5beec691bf4541541dfc73c6d8322801276a01387d10";
    // Generated by OpenSSL 3.2.2 over CANONICAL_HEX. The private key is not
    // stored in the repository; these are independent known-answer inputs.
    const PUBLIC_KEY_DER_HEX: &str = "30820122300d06092a864886f70d01010105000382010f003082010a0282010100b9b2ca105b54d49dfa23a4080ade76d0328e24c4aa216acdd7a19d57812718d88876afa7a8aadef958c3e0408cc665f21fedf43eb02f860aa0c5820b1920b36735a72940c9dd48ac44f4f27f5031993cec9a3d014ce07123fd4c99a3d21642c34167b062e495a537d41bafce23cfea9f411be4cc0aa1fd49dc6ea49fc2331d26203368baa3c47ca808b1a83b07bf87d60e2240e51235cdafd18fc84b631cafc4010fb2725fceedcb8b25b04cdf103f9414c187b4e6e3885050da998d81b3fdd8559d2caeb8360059552203f956cea3e5e858bcf869ebac3b3fc3d54c9a99c4ea2d7582976bc10a01de952cbede994254b7d7e4fb343143d9336cca80f71fba610203010001";
    const SIGNATURE_HEX: &str = "111e5237ce289b06afef21993b38f67e5ec9155e927cfb0b68b78edd518bfad4649c7b05d9fd5120c6abb0b8753293dce79f22743ef91acf293a4b46eb31568fdf9fb8de8e6eea8b0a6e3b90c79897625d8c458aa9d89aadd6113f4a066d02f011cbb89e4238a24bace081f94e6c4d15679dcdf4512bf82f65a6f3546164442639a979bac23470dd2780ab5c512215d016a0b9da65edbc2bf267b9e5ef27679381b1f025d64c1d44c55b26d74787e03003ec5e41fe3032953890886aa47786fbd9c6641cbdd954826cc9f71daee64f3312a49bcd47653306c8b4ba37ae94121b9bc350246ac590f0fb675c7f117212dd69af74dd8361881219ca1365906c8ab0";

    fn fixture() -> (SignedMessageLink, SignedMessageBody) {
        (
            SignedMessageLink::new(0, Uuid::from_u128(1), Uuid::from_u128(2)),
            SignedMessageBody::new(
                "hi".to_string(),
                1_700_000_000_000,
                0x0102_0304_0506_0708,
                Vec::new(),
            ),
        )
    }

    fn fixture_bytes(hex_value: &str) -> Vec<u8> {
        hex::decode(hex_value).unwrap_or_default()
    }

    fn independent_body(
        packet: &Value,
        argument: &Value,
    ) -> (SignedMessageLink, SignedMessageBody) {
        let link = &argument["link"];
        let sender = Uuid::parse_str(link["sender_uuid"].as_str().expect("sender UUID"))
            .expect("sender UUID is valid");
        let session = Uuid::parse_str(link["session_uuid"].as_str().expect("session UUID"))
            .expect("session UUID is valid");
        let salt = i64::from_str_radix(
            packet["salt_i64"]
                .as_str()
                .expect("salt is a hexadecimal string")
                .trim_start_matches("0x"),
            16,
        )
        .expect("salt is valid hexadecimal");
        let last_seen = packet["last_seen_signature_hex"]
            .as_array()
            .expect("last-seen signatures")
            .iter()
            .map(|signature| {
                fixture_bytes(signature.as_str().expect("last-seen signature hex"))
                    .into_boxed_slice()
            })
            .collect();
        let raw_value = fixture_bytes(
            argument["raw_value_utf8_hex"]
                .as_str()
                .expect("raw argument UTF-8"),
        );
        assert_eq!(
            String::from_utf8(raw_value.clone()).expect("raw argument is UTF-8"),
            argument["raw_value_text"]
                .as_str()
                .expect("raw argument value")
        );
        (
            SignedMessageLink::new(
                link["index"].as_i64().expect("link index") as i32,
                sender,
                session,
            ),
            SignedMessageBody::new(
                String::from_utf8(raw_value).expect("raw argument is UTF-8"),
                packet["timestamp_epoch_second"]
                    .as_i64()
                    .expect("timestamp")
                    * 1_000,
                salt,
                last_seen,
            ),
        )
    }

    #[test]
    fn independent_signed_command_fixture_verifies_positive_and_negative_cases() {
        let fixture: Value = serde_json::from_str(INDEPENDENT_FIXTURE).expect("fixture JSON");
        let public_key = fixture_bytes(
            fixture["crypto"]["public_key_der_hex"]
                .as_str()
                .expect("public key DER"),
        );
        let vectors = fixture["vectors"].as_array().expect("fixture vectors");
        assert_eq!(vectors.len(), 5);
        let mut argument_count = 0;

        for vector in vectors {
            let packet = &vector["packet"];
            for argument in vector["arguments"].as_array().expect("fixture arguments") {
                argument_count += 1;
                let (link, body) = independent_body(packet, argument);
                let expected_canonical =
                    fixture_bytes(argument["canonical_hex"].as_str().expect("canonical bytes"));
                let canonical = canonical_bytes(&link, &body).expect("canonical bytes build");
                assert_eq!(
                    canonical.len(),
                    argument["canonical_length"].as_u64().unwrap() as usize
                );
                assert_eq!(canonical, expected_canonical);
                assert_eq!(
                    hex::encode(Sha256::digest(&canonical)),
                    argument["canonical_sha256"].as_str().unwrap()
                );
                assert_eq!(
                    verify_chat_message_signature(
                        &public_key,
                        &link,
                        &body,
                        &fixture_bytes(argument["signature_hex"].as_str().unwrap()),
                    ),
                    Ok(())
                );

                let mut tampered_body = body.clone();
                tampered_body.content.push('!');
                assert_eq!(
                    verify_chat_message_signature(
                        &public_key,
                        &link,
                        &tampered_body,
                        &fixture_bytes(argument["signature_hex"].as_str().unwrap()),
                    ),
                    Err(ChatSignatureError::SignatureMismatch)
                );

                let tampered_link =
                    SignedMessageLink::new(link.index + 1, link.sender, link.session_id);
                assert_eq!(
                    verify_chat_message_signature(
                        &public_key,
                        &tampered_link,
                        &body,
                        &fixture_bytes(argument["signature_hex"].as_str().unwrap()),
                    ),
                    Err(ChatSignatureError::SignatureMismatch)
                );
            }
        }
        assert_eq!(argument_count, 6);

        let n2 = vectors
            .iter()
            .find(|vector| vector["n"].as_u64() == Some(2))
            .expect("N=2 vector");
        let args = n2["arguments"].as_array().expect("N=2 arguments");
        let (first_link, _) = independent_body(&n2["packet"], &args[0]);
        let (_, second_body) = independent_body(&n2["packet"], &args[1]);
        assert_eq!(n2["entry_order"][0].as_str(), Some("first"));
        assert_eq!(n2["entry_order"][1].as_str(), Some("rest"));
        let reordered_link =
            SignedMessageLink::new(first_link.index, first_link.sender, first_link.session_id);
        assert_eq!(
            verify_chat_message_signature(
                &public_key,
                &reordered_link,
                &second_body,
                &fixture_bytes(args[1]["signature_hex"].as_str().unwrap()),
            ),
            Err(ChatSignatureError::SignatureMismatch)
        );

        let mut wrong_key = public_key.clone();
        wrong_key[30] ^= 1;
        let (link, body) = independent_body(&vectors[0]["packet"], &vectors[0]["arguments"][0]);
        assert!(matches!(
            verify_chat_message_signature(
                &wrong_key,
                &link,
                &body,
                &fixture_bytes(
                    vectors[0]["arguments"][0]["signature_hex"]
                        .as_str()
                        .unwrap()
                ),
            ),
            Err(ChatSignatureError::InvalidPublicKey | ChatSignatureError::SignatureMismatch)
        ));
    }

    #[test]
    fn canonical_empty_last_seen_matches_known_answer() {
        let (link, body) = fixture();
        let payload = canonical_bytes(&link, &body);
        assert_eq!(payload, Ok(fixture_bytes(CANONICAL_HEX)));
        let payload = payload.unwrap_or_default();
        assert_eq!(payload.len(), 66);
        assert_eq!(hex::encode(Sha256::digest(&payload)), CANONICAL_SHA256_HEX);
    }

    #[test]
    fn independent_signature_verifies() {
        let (link, body) = fixture();
        assert_eq!(
            verify_chat_message_signature(
                &fixture_bytes(PUBLIC_KEY_DER_HEX),
                &link,
                &body,
                &fixture_bytes(SIGNATURE_HEX),
            ),
            Ok(())
        );
    }

    #[test]
    fn content_change_is_rejected() {
        let (link, mut body) = fixture();
        body.content.push('!');
        assert_eq!(
            verify_chat_message_signature(
                &fixture_bytes(PUBLIC_KEY_DER_HEX),
                &link,
                &body,
                &fixture_bytes(SIGNATURE_HEX)
            ),
            Err(ChatSignatureError::SignatureMismatch)
        );
    }

    #[test]
    fn salt_change_is_rejected() {
        let (link, mut body) = fixture();
        body.salt += 1;
        assert_eq!(
            verify_chat_message_signature(
                &fixture_bytes(PUBLIC_KEY_DER_HEX),
                &link,
                &body,
                &fixture_bytes(SIGNATURE_HEX)
            ),
            Err(ChatSignatureError::SignatureMismatch)
        );
    }

    #[test]
    fn timestamp_change_is_rejected() {
        let (link, mut body) = fixture();
        body.time_stamp += 1_000;
        assert_eq!(
            verify_chat_message_signature(
                &fixture_bytes(PUBLIC_KEY_DER_HEX),
                &link,
                &body,
                &fixture_bytes(SIGNATURE_HEX)
            ),
            Err(ChatSignatureError::SignatureMismatch)
        );
    }

    #[test]
    fn session_change_is_rejected() {
        let (mut link, body) = fixture();
        link.session_id = Uuid::from_u128(3);
        assert_eq!(
            verify_chat_message_signature(
                &fixture_bytes(PUBLIC_KEY_DER_HEX),
                &link,
                &body,
                &fixture_bytes(SIGNATURE_HEX)
            ),
            Err(ChatSignatureError::SignatureMismatch)
        );
    }

    #[test]
    fn index_change_is_rejected() {
        let (mut link, body) = fixture();
        link.index = 1;
        assert_eq!(
            verify_chat_message_signature(
                &fixture_bytes(PUBLIC_KEY_DER_HEX),
                &link,
                &body,
                &fixture_bytes(SIGNATURE_HEX)
            ),
            Err(ChatSignatureError::SignatureMismatch)
        );
    }

    #[test]
    fn last_seen_change_is_rejected() {
        let (link, mut body) = fixture();
        body.last_seen
            .push(vec![0_u8; CHAT_SIGNATURE_LEN].into_boxed_slice());
        assert_eq!(
            verify_chat_message_signature(
                &fixture_bytes(PUBLIC_KEY_DER_HEX),
                &link,
                &body,
                &fixture_bytes(SIGNATURE_HEX)
            ),
            Err(ChatSignatureError::SignatureMismatch)
        );
    }

    #[test]
    fn wrong_key_is_rejected() {
        let (link, body) = fixture();
        let mut public_key = fixture_bytes(PUBLIC_KEY_DER_HEX);
        if let Some(byte) = public_key.get_mut(30) {
            *byte ^= 1;
        }
        assert!(matches!(
            verify_chat_message_signature(&public_key, &link, &body, &fixture_bytes(SIGNATURE_HEX)),
            Err(ChatSignatureError::InvalidPublicKey | ChatSignatureError::SignatureMismatch)
        ));
    }

    #[test]
    fn zero_signature_is_rejected() {
        let (link, body) = fixture();
        assert_eq!(
            verify_chat_message_signature(
                &fixture_bytes(PUBLIC_KEY_DER_HEX),
                &link,
                &body,
                &[0; CHAT_SIGNATURE_LEN]
            ),
            Err(ChatSignatureError::SignatureMismatch)
        );
    }

    #[test]
    fn invalid_signature_length_is_rejected() {
        let (link, body) = fixture();
        assert_eq!(
            verify_chat_message_signature(
                &fixture_bytes(PUBLIC_KEY_DER_HEX),
                &link,
                &body,
                &[0; CHAT_SIGNATURE_LEN - 1]
            ),
            Err(ChatSignatureError::InvalidSignatureLength {
                length: CHAT_SIGNATURE_LEN - 1,
                expected: CHAT_SIGNATURE_LEN
            })
        );
    }
}
