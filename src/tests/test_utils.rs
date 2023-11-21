use bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};
use bytes::BufMut;
use lightning::ln::msgs::OnionMessage;
use lightning::util::ser::Readable;
//use lightning::util::ser::Writeable;
use std::io::Cursor;

pub fn pubkey(byte: u8) -> PublicKey {
    let secp_ctx = Secp256k1::new();
    PublicKey::from_secret_key(&secp_ctx, &privkey(42 + byte))
}

pub fn privkey(byte: u8) -> SecretKey {
    SecretKey::from_slice(&[byte; 32]).unwrap()
}

/// Produces an OnionMessage that can be used for tests. We need to manually write individual bytes because onion
/// messages in LDK can only be created using read/write impls that deal with raw bytes (since some other fields
/// are not public).
pub fn onion_message() -> OnionMessage {
    let mut w = vec![];
    let pubkey_bytes = pubkey(0).serialize();

    // Blinding point for the onion message.
    w.put_slice(&pubkey_bytes);

    // Write the length of the onion packet:
    // Version: 1
    // Ephemeral Key: 33
    // Hop Payloads: 1300
    // HMAC: 32.
    w.put_u16(1 + 33 + 1300 + 32);

    // Write meaningless contents for the actual values.
    w.put_u8(0);
    w.put_slice(&pubkey_bytes);
    w.put_bytes(1, 1300);
    w.put_bytes(2, 32);

    let mut readable = Cursor::new(w);
    OnionMessage::read(&mut readable).unwrap()
}
