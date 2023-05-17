use bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};

pub fn pubkey(byte: u8) -> PublicKey {
    let secp_ctx = Secp256k1::new();
    PublicKey::from_secret_key(&secp_ctx, &privkey(42 + byte))
}

pub fn privkey(byte: u8) -> SecretKey {
    SecretKey::from_slice(&[byte; 32]).unwrap()
}
